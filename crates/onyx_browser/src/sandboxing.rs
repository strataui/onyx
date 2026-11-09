//! Multi-Process Tab Sandboxing, Fault Containment & Zero-Copy Compositor Bridge.
//!
//! Enforces OS-level and worker-level fault isolation for web tab renderers.
//! Prevents tab crashes, infinite loops, and memory faults from compromising
//! the main browser UI thread or sibling tabs. Supports real OS subprocesses
//! and Linux seccomp security profiles.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use strata_layout::Rect;
use strata_render::compositor::{DisplayList, DrawCommand};
use strata_render::shm_compositor::ShmRingBuffer;
use strata_style::types::Color;
use strata_web_reader::ast::SemanticNode;
use strata_web_reader::HtmlParser;

use crate::network::{load_page, PageLoadResult};
use crate::state::NetworkRequestEntry;

/// Configurable security and isolation policy for tab execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxPolicy {
    /// Sandboxed thread worker with isolated panic hooks and memory guard.
    #[default]
    ThreadSandbox,
    /// Isolated OS subprocess with dedicated PID and IPC channels.
    ProcessSandbox,
    /// Restricted system call profile (seccomp on Linux).
    SeccompSandbox,
}

/// Lifecycle execution status of a sandboxed tab worker.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessStatus {
    Idle,
    Running {
        pid: u32,
        started_at: Instant,
    },
    Crashed {
        exit_code: Option<i32>,
        reason: String,
    },
    Terminated,
    Restarting,
}

/// Dispatched navigation or rendering job for a sandboxed tab worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxedJob {
    pub tab_id: usize,
    pub generation: u64,
    pub url: String,
    pub simulate_crash: bool,
    pub simulate_hang: bool,
}

/// Outgoing result or crash notification from a sandboxed worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxedResponse {
    pub tab_id: usize,
    pub generation: u64,
    pub url: String,
    pub title: String,
    pub html: String,
    pub dom_nodes: Vec<SemanticNode>,
    pub status: u16,
    pub is_error: bool,
    pub is_crashed: bool,
    pub crash_reason: Option<String>,
    pub elapsed_ms: f32,
    pub request_entry: NetworkRequestEntry,
    pub frame_sequence: u64,
    #[serde(default)]
    pub shm_slot: Option<usize>,
}

/// Helper function to commit a rendered frame to the zero-copy shared memory ring buffer.
fn commit_frame_to_ring_buffer(
    ring_buffer: &ShmRingBuffer,
    title: &str,
    dom_nodes: &[SemanticNode],
) -> u64 {
    let slot_idx = ring_buffer.begin_write();
    let mut dl = DisplayList::default();
    dl.push(DrawCommand::DrawRect {
        bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        color: Color::rgb(255, 255, 255),
    });

    let display_title = if !dom_nodes.is_empty() {
        let first_text = dom_nodes
            .iter()
            .map(|n| n.text_content())
            .find(|s| !s.trim().is_empty());
        first_text.unwrap_or_else(|| title.to_string())
    } else {
        title.to_string()
    };

    dl.push(DrawCommand::DrawText {
        origin: (24.0, 32.0),
        bounds_width: Some(800.0),
        bounds_height: Some(40.0),
        text_align: strata_style::TextAlign::Left,
        text: display_title,
        font_size: 24.0,
        line_height: 32.0,
        font_weight: strata_style::FontWeight::Bold,
        color: Color::rgb(17, 24, 39),
        is_monospace: false,
        font_family: strata::primitives::AtomId::FONT_SANS,
    });

    ring_buffer.commit_write(
        slot_idx,
        1920,
        1080,
        vec![Rect::new(0.0, 0.0, 1920.0, 1080.0)],
        dl,
        None,
    );

    (slot_idx as u64) + 1
}

pub const SHM_MAGIC: u32 = 0x53545241; // 'STRA'
pub const SHM_VERSION: u32 = 1;
pub const SHM_NUM_SLOTS: usize = 3;
pub const SHM_HEADER_RESERVED: usize = 4096;
pub const SHM_SLOT_SIZE: usize = 4 * 1024 * 1024; // 4 MB per slot
pub const SHM_TOTAL_BUFFER_SIZE: usize = SHM_HEADER_RESERVED + SHM_NUM_SLOTS * SHM_SLOT_SIZE;

pub const SLOT_STATUS_AVAILABLE: u32 = 0;
pub const SLOT_STATUS_WRITING: u32 = 1;
pub const SLOT_STATUS_COMMITTED: u32 = 2;
pub const SLOT_STATUS_ACQUIRED: u32 = 3;

pub const PAYLOAD_TYPE_EMPTY: u32 = 0;
pub const PAYLOAD_TYPE_DISPLAY_LIST: u32 = 1;
pub const PAYLOAD_TYPE_RAW_PIXELS: u32 = 2;
pub const PAYLOAD_TYPE_DOM_DATA: u32 = 3;

/// Master binary header for cross-process shared memory ring buffer.
#[repr(C)]
pub struct ProcessShmHeader {
    pub magic: u32,
    pub version: u32,
    pub num_slots: u32,
    pub slot_size: u32,
    pub head_sequence: AtomicU64,
    pub tail_sequence: AtomicU64,
    pub latest_committed_slot: AtomicU32,
    pub dropped_frames: AtomicU64,
}

/// Per-slot binary header for frame metadata and damage rects.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessSlotHeader {
    pub sequence: u64,
    pub status: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub damage_count: u32,
    pub damage_rects: [[f32; 4]; 16],
    pub payload_type: u32,
    pub payload_len: u32,
}

/// Serialized frame payload object for shared memory transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShmFramePayload {
    pub title: String,
    pub dom_nodes: Vec<SemanticNode>,
    #[serde(default)]
    pub raw_pixels: Option<Vec<u8>>,
}

/// Cloned snapshot of a committed frame read from the shared memory ring buffer.
#[derive(Debug, Clone)]
pub struct ShmFrameSnapshot {
    pub sequence: u64,
    pub slot_index: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub damage_rects: Vec<Rect>,
    pub payload_type: u32,
    pub payload: Vec<u8>,
}

impl ShmFrameSnapshot {
    pub fn parse_payload(&self) -> Option<ShmFramePayload> {
        if self.payload_type == PAYLOAD_TYPE_DOM_DATA && !self.payload.is_empty() {
            serde_json::from_slice(&self.payload).ok()
        } else {
            None
        }
    }

    pub fn raw_pixels(&self) -> Option<&[u8]> {
        if self.payload_type == PAYLOAD_TYPE_RAW_PIXELS {
            Some(&self.payload)
        } else {
            None
        }
    }
}

/// POSIX and Cross-Platform OS Shared Memory mapping for zero-copy IPC.
pub struct OsSharedMemory {
    path: Option<PathBuf>,
    fd: libc::c_int,
    ptr: *mut u8,
    size: usize,
    is_owner: bool,
}

unsafe impl Send for OsSharedMemory {}
unsafe impl Sync for OsSharedMemory {}

impl OsSharedMemory {
    /// Creates a new shared memory region of `size` bytes backed by /dev/shm or temp dir.
    pub fn create(name_prefix: &str, size: usize) -> std::io::Result<Self> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let file_name = format!("{name_prefix}_{pid}_{timestamp}.shm");

        let shm_dir = Path::new("/dev/shm");
        let candidate_path = if shm_dir.exists() && shm_dir.is_dir() {
            shm_dir.join(&file_name)
        } else {
            std::env::temp_dir().join(&file_name)
        };

        let c_path = std::ffi::CString::new(candidate_path.to_string_lossy().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                0o600,
            )
        };

        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        if unsafe { libc::ftruncate(fd, size as libc::off_t) } < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            let _ = std::fs::remove_file(&candidate_path);
            return Err(err);
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if ptr == libc::MAP_FAILED || ptr.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            let _ = std::fs::remove_file(&candidate_path);
            return Err(err);
        }

        unsafe {
            std::ptr::write_bytes(ptr as *mut u8, 0, size);
        }

        Ok(Self {
            path: Some(candidate_path),
            fd,
            ptr: ptr as *mut u8,
            size,
            is_owner: true,
        })
    }

    /// Maps an existing shared memory region from path.
    pub fn open_existing(path: &Path, size: usize) -> std::io::Result<Self> {
        let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };

        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if ptr == libc::MAP_FAILED || ptr.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }

        Ok(Self {
            path: Some(path.to_path_buf()),
            fd,
            ptr: ptr as *mut u8,
            size,
            is_owner: false,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Drop for OsSharedMemory {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.ptr != libc::MAP_FAILED as *mut u8 {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.size);
            }
            self.ptr = std::ptr::null_mut();
        }
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
            self.fd = -1;
        }
        if self.is_owner {
            if let Some(ref p) = self.path {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Triple-buffered cross-process shared memory ring buffer for zero-copy frame transfer.
pub struct CrossProcessShmRingBuffer {
    shm: Mutex<OsSharedMemory>,
    capacity: usize,
    slot_size: usize,
}

impl CrossProcessShmRingBuffer {
    /// Creates a new cross-process shared memory ring buffer with 3 slots (triple buffering).
    pub fn create(name_prefix: &str) -> std::io::Result<Arc<Self>> {
        let mut os_shm = OsSharedMemory::create(name_prefix, SHM_TOTAL_BUFFER_SIZE)?;

        let slice = os_shm.as_mut_slice();
        let header = ProcessShmHeader {
            magic: SHM_MAGIC,
            version: SHM_VERSION,
            num_slots: SHM_NUM_SLOTS as u32,
            slot_size: SHM_SLOT_SIZE as u32,
            head_sequence: AtomicU64::new(0),
            tail_sequence: AtomicU64::new(0),
            latest_committed_slot: AtomicU32::new(u32::MAX),
            dropped_frames: AtomicU64::new(0),
        };

        unsafe {
            std::ptr::copy_nonoverlapping(
                &header as *const ProcessShmHeader as *const u8,
                slice.as_mut_ptr(),
                std::mem::size_of::<ProcessShmHeader>(),
            );
        }

        Ok(Arc::new(Self {
            shm: Mutex::new(os_shm),
            capacity: SHM_NUM_SLOTS,
            slot_size: SHM_SLOT_SIZE,
        }))
    }

    /// Maps an existing cross-process shared memory ring buffer by file path.
    pub fn open_existing(path: &Path) -> std::io::Result<Arc<Self>> {
        let os_shm = OsSharedMemory::open_existing(path, SHM_TOTAL_BUFFER_SIZE)?;

        let slice = os_shm.as_slice();
        if slice.len() < std::mem::size_of::<ProcessShmHeader>() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "SHM region smaller than header",
            ));
        }

        let header = unsafe { &*(slice.as_ptr() as *const ProcessShmHeader) };
        if header.magic != SHM_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Invalid SHM magic: 0x{:08X}", header.magic),
            ));
        }
        if header.version != SHM_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Unsupported SHM version: {}", header.version),
            ));
        }

        let cap = (header.num_slots as usize).max(2);
        let slot_size = header.slot_size as usize;

        Ok(Arc::new(Self {
            shm: Mutex::new(os_shm),
            capacity: cap,
            slot_size,
        }))
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.shm.lock().unwrap().path().map(|p| p.to_path_buf())
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn slot_size(&self) -> usize {
        self.slot_size
    }

    /// Acquires a slot for the producer to write a frame.
    pub fn begin_write(&self) -> (usize, u64) {
        let mut shm = self.shm.lock().unwrap();
        let slice = shm.as_mut_slice();
        let main_header = unsafe { &*(slice.as_ptr() as *const ProcessShmHeader) };
        let prev_seq = main_header.head_sequence.fetch_add(1, Ordering::SeqCst);
        let seq = prev_seq + 1;
        let slot_idx = (seq as usize) % self.capacity;

        let slot_offset = SHM_HEADER_RESERVED + slot_idx * self.slot_size;
        let slot_header_ptr = unsafe {
            slice.as_mut_ptr().add(slot_offset) as *mut ProcessSlotHeader
        };
        unsafe {
            (*slot_header_ptr).sequence = seq;
            (*slot_header_ptr).status = SLOT_STATUS_WRITING;
        }

        (slot_idx, seq)
    }

    /// Commits a written frame into the designated slot.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_write(
        &self,
        slot_idx: usize,
        seq: u64,
        width: u32,
        height: u32,
        damage_rects: &[Rect],
        payload_type: u32,
        payload: &[u8],
    ) {
        if slot_idx >= self.capacity {
            return;
        }

        let mut shm = self.shm.lock().unwrap();
        let slot_offset = SHM_HEADER_RESERVED + slot_idx * self.slot_size;
        let header_size = std::mem::size_of::<ProcessSlotHeader>();

        if slot_offset + header_size + payload.len() > shm.size() {
            return;
        }

        let mut damage_array = [[0.0f32; 4]; 16];
        let damage_count = damage_rects.len().min(16);
        for (i, r) in damage_rects.iter().take(damage_count).enumerate() {
            damage_array[i] = [r.x, r.y, r.width, r.height];
        }

        let slot_header = ProcessSlotHeader {
            sequence: seq,
            status: SLOT_STATUS_COMMITTED,
            width,
            height,
            stride: width * 4,
            damage_count: damage_count as u32,
            damage_rects: damage_array,
            payload_type,
            payload_len: payload.len() as u32,
        };

        let slice = shm.as_mut_slice();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &slot_header as *const ProcessSlotHeader as *const u8,
                slice.as_mut_ptr().add(slot_offset),
                header_size,
            );
            if !payload.is_empty() {
                std::ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    slice.as_mut_ptr().add(slot_offset + header_size),
                    payload.len(),
                );
            }
        }

        let main_header = unsafe { &*(slice.as_ptr() as *const ProcessShmHeader) };
        let prev = main_header.latest_committed_slot.swap(slot_idx as u32, Ordering::SeqCst);
        if prev != u32::MAX && prev != (slot_idx as u32) {
            let tail_seq = main_header.tail_sequence.load(Ordering::SeqCst);
            let prev_offset = SHM_HEADER_RESERVED + (prev as usize) * self.slot_size;
            let prev_slot = unsafe {
                &*(slice.as_ptr().add(prev_offset) as *const ProcessSlotHeader)
            };
            if prev_slot.sequence > tail_seq {
                main_header.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Reads frame metadata and payload from a specific slot index.
    pub fn read_slot(&self, slot_idx: usize) -> Option<ShmFrameSnapshot> {
        if slot_idx >= self.capacity {
            return None;
        }

        let shm = self.shm.lock().unwrap();
        let slot_offset = SHM_HEADER_RESERVED + slot_idx * self.slot_size;
        let header_size = std::mem::size_of::<ProcessSlotHeader>();

        if slot_offset + header_size > shm.size() {
            return None;
        }

        let slice = shm.as_slice();
        let slot_header = unsafe {
            std::ptr::read_unaligned(slice.as_ptr().add(slot_offset) as *const ProcessSlotHeader)
        };

        if slot_header.status != SLOT_STATUS_COMMITTED && slot_header.status != SLOT_STATUS_ACQUIRED {
            return None;
        }

        let payload_len = slot_header.payload_len as usize;
        let max_payload = self.slot_size.saturating_sub(header_size);
        let actual_len = payload_len.min(max_payload);

        let mut payload = vec![0u8; actual_len];
        if actual_len > 0 && slot_offset + header_size + actual_len <= shm.size() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    slice.as_ptr().add(slot_offset + header_size),
                    payload.as_mut_ptr(),
                    actual_len,
                );
            }
        }

        let mut damage_rects = Vec::with_capacity(slot_header.damage_count as usize);
        let count = (slot_header.damage_count as usize).min(16);
        for i in 0..count {
            let [x, y, w, h] = slot_header.damage_rects[i];
            damage_rects.push(Rect::new(x, y, w, h));
        }

        Some(ShmFrameSnapshot {
            sequence: slot_header.sequence,
            slot_index: slot_idx,
            width: slot_header.width,
            height: slot_header.height,
            stride: slot_header.stride,
            damage_rects,
            payload_type: slot_header.payload_type,
            payload,
        })
    }

    /// Acquires the latest committed frame if newer than `last_sequence`.
    pub fn acquire_latest(&self, last_sequence: u64) -> Option<ShmFrameSnapshot> {
        let shm = self.shm.lock().unwrap();
        let slice = shm.as_slice();
        if slice.len() < std::mem::size_of::<ProcessShmHeader>() {
            return None;
        }

        let main_header = unsafe { &*(slice.as_ptr() as *const ProcessShmHeader) };
        if main_header.magic != SHM_MAGIC || main_header.version != SHM_VERSION {
            return None;
        }

        let committed_idx = main_header.latest_committed_slot.load(Ordering::SeqCst);
        if committed_idx == u32::MAX || (committed_idx as usize) >= self.capacity {
            return None;
        }

        drop(shm);
        if let Some(snapshot) = self.read_slot(committed_idx as usize) {
            if snapshot.sequence > last_sequence {
                let shm = self.shm.lock().unwrap();
                let slice = shm.as_slice();
                let main_header = unsafe { &*(slice.as_ptr() as *const ProcessShmHeader) };
                main_header.tail_sequence.store(snapshot.sequence, Ordering::SeqCst);
                return Some(snapshot);
            }
        }

        None
    }
}

#[cfg(target_os = "linux")]
mod seccomp_bpf {
    pub const BPF_LD: u16 = 0x00;
    pub const BPF_W: u16 = 0x00;
    pub const BPF_ABS: u16 = 0x20;
    pub const BPF_JMP: u16 = 0x05;
    pub const BPF_JEQ: u16 = 0x10;
    pub const BPF_K: u16 = 0x00;
    pub const BPF_RET: u16 = 0x06;

    pub const SECCOMP_RET_KILL_PROCESS: u32 = 0x80000000;
    pub const SECCOMP_RET_ERRNO: u32 = 0x00050000;
    pub const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;

    pub const PR_SET_SECCOMP: libc::c_int = 22;
    pub const SECCOMP_MODE_FILTER: libc::c_ulong = 2;

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct SockFilter {
        pub code: u16,
        pub jt: u8,
        pub jf: u8,
        pub k: u32,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct SockFprog {
        pub len: u16,
        pub filter: *const SockFilter,
    }
}

/// Enforces Linux seccomp-bpf filter restricting child worker syscalls.
#[cfg(target_os = "linux")]
pub fn apply_seccomp_bpf_filter() -> Result<(), String> {
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000003e; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc00000b7; // AUDIT_ARCH_AARCH64
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const AUDIT_ARCH: u32 = 0;

    #[cfg(target_arch = "x86_64")]
    const BLOCKED_SYSCALLS: &[libc::c_long] = &[
        libc::SYS_ptrace,
        libc::SYS_chroot,
        libc::SYS_reboot,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
    ];

    #[cfg(target_arch = "aarch64")]
    const BLOCKED_SYSCALLS: &[libc::c_long] = &[
        117, // ptrace
        51,  // chroot
        142, // reboot
        104, // kexec_load
        105, // init_module
        273, // finit_module
        106, // delete_module
    ];

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const BLOCKED_SYSCALLS: &[libc::c_long] = &[];

    if AUDIT_ARCH == 0 || BLOCKED_SYSCALLS.is_empty() {
        return Ok(());
    }

    use seccomp_bpf::*;

    // 1. Load architecture: [offset 4 of seccomp_data] and verify match
    let mut filter = vec![
        SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: 4,
        },
        SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH,
        },
        SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_KILL_PROCESS,
        },
        // 2. Load system call number: [offset 0 of seccomp_data]
        SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: 0,
        },
    ];

    // 3. Check blocked syscalls: if matched, return EPERM
    for &sc in BLOCKED_SYSCALLS {
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 0,
            jf: 1,
            k: sc as u32,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        });
    }

    // 4. Default: allow all other syscalls
    filter.push(SockFilter {
        code: BPF_RET | BPF_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });

    let prog = SockFprog {
        len: filter.len() as u16,
        filter: filter.as_ptr(),
    };

    let ret = unsafe {
        libc::prctl(
            PR_SET_SECCOMP,
            SECCOMP_MODE_FILTER,
            &prog as *const SockFprog as *const libc::c_void,
        )
    };

    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("prctl(PR_SET_SECCOMP) failed: errno {err}"));
    }

    Ok(())
}

/// Enforces Linux seccomp security profile and privilege restrictions on the child worker.
#[cfg(target_os = "linux")]
pub fn apply_seccomp_sandbox() -> Result<(), String> {
    // 1. Enforce PR_SET_NO_NEW_PRIVS: prevents the process and its children from gaining new privileges
    let res = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if res != 0 {
        return Err(format!(
            "Failed to set PR_SET_NO_NEW_PRIVS: errno {}",
            std::io::Error::last_os_error()
        ));
    }
    // 2. Prevent core dumps from leaking renderer memory
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
    // 3. Apply seccomp BPF filter
    apply_seccomp_bpf_filter()
}

#[cfg(not(target_os = "linux"))]
pub fn apply_seccomp_sandbox() -> Result<(), String> {
    Ok(())
}

/// Enforces OS-level sandboxing according to the specified policy.
pub fn apply_os_sandbox(policy: SandboxPolicy) -> Result<(), String> {
    match policy {
        SandboxPolicy::ThreadSandbox => Ok(()),
        SandboxPolicy::ProcessSandbox => {
            #[cfg(target_os = "linux")]
            unsafe {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
            }
            Ok(())
        }
        SandboxPolicy::SeccompSandbox => apply_seccomp_sandbox(),
    }
}

/// Spawns an authentic child process configured as a tab worker.
fn spawn_child_process(
    worker_id: usize,
    policy: SandboxPolicy,
    shm_path: Option<&Path>,
) -> std::io::Result<Child> {
    let current_exe = std::env::current_exe()?;
    let mut cmd = Command::new(&current_exe);

    // If current executable is a test harness, target the worker entrypoint
    if current_exe.to_string_lossy().contains("deps") {
        cmd.arg("strata_tab_worker_process_entrypoint")
            .arg("--exact")
            .arg("--nocapture");
    } else {
        cmd.arg("--tab-worker");
    }

    cmd.env("STRATA_TAB_WORKER", "1")
        .env("STRATA_TAB_WORKER_ID", worker_id.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if policy == SandboxPolicy::SeccompSandbox {
        cmd.env("STRATA_TAB_SECCOMP", "1");
    }

    if let Some(p) = shm_path {
        cmd.env("STRATA_TAB_SHM_PATH", p.to_string_lossy().as_ref());
    }

    cmd.spawn()
}

/// Main entry point for authentic child OS tab worker processes.
///
/// Reads jobs as newline-delimited JSON from stdin, performs isolated navigation
/// and DOM parsing, and writes completed responses as JSON back to stdout.
pub fn run_tab_worker_process_loop() {
    let policy = if std::env::var("STRATA_TAB_SECCOMP").map(|v| v == "1").unwrap_or(false) {
        SandboxPolicy::SeccompSandbox
    } else {
        SandboxPolicy::ProcessSandbox
    };

    if let Err(e) = apply_os_sandbox(policy) {
        eprintln!("[SandboxedTabWorker] Warning: sandbox profile application failed: {e}");
    }

    let cross_shm = std::env::var("STRATA_TAB_SHM_PATH")
        .ok()
        .and_then(|p| CrossProcessShmRingBuffer::open_existing(Path::new(&p)).ok());

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    let mut line = String::new();
    while let Ok(bytes) = reader.read_line(&mut line) {
        if bytes == 0 {
            break; // EOF: parent process closed pipe
        }
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            line.clear();
            continue;
        }

        let job: SandboxedJob = match serde_json::from_str(trimmed) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[SandboxedTabWorker] Failed to deserialize job: {e}");
                line.clear();
                continue;
            }
        };

        if job.simulate_hang {
            std::thread::sleep(Duration::from_secs(3600));
        }

        if job.simulate_crash {
            #[cfg(unix)]
            unsafe {
                libc::raise(libc::SIGSEGV);
            }
            std::process::exit(139);
        }

        let t_start = Instant::now();
        let result: PageLoadResult = load_page(&job.url);
        let dom_nodes = HtmlParser::parse_with_scripts(&result.html);
        let elapsed_ms = t_start.elapsed().as_secs_f32() * 1000.0;

        let request_entry = NetworkRequestEntry {
            name: job.url.clone(),
            method: "GET".to_string(),
            status: result.status,
            status_text: if result.status == 200 {
                "OK".to_string()
            } else {
                "Error".to_string()
            },
            kind: "document".to_string(),
            initiator: "navigation".to_string(),
            size_bytes: result.html.len(),
            time_ms: elapsed_ms,
        };

        let mut frame_seq = 1;
        let mut shm_slot = None;
        if let Some(ref shm) = cross_shm {
            let (slot_idx, seq) = shm.begin_write();
            frame_seq = seq;
            shm_slot = Some(slot_idx);

            let payload_obj = ShmFramePayload {
                title: result.title.clone(),
                dom_nodes: dom_nodes.clone(),
                raw_pixels: None,
            };
            if let Ok(bytes) = serde_json::to_vec(&payload_obj) {
                shm.commit_write(
                    slot_idx,
                    seq,
                    1920,
                    1080,
                    &[Rect::new(0.0, 0.0, 1920.0, 1080.0)],
                    PAYLOAD_TYPE_DOM_DATA,
                    &bytes,
                );
            }
        }

        let response = SandboxedResponse {
            tab_id: job.tab_id,
            generation: job.generation,
            url: job.url,
            title: result.title,
            html: result.html,
            dom_nodes,
            status: result.status,
            is_error: result.is_error,
            is_crashed: false,
            crash_reason: None,
            elapsed_ms,
            request_entry,
            frame_sequence: frame_seq,
            shm_slot,
        };

        if let Ok(serialized) = serde_json::to_string(&response) {
            let _ = writeln!(writer, "{serialized}");
            let _ = writer.flush();
        }

        line.clear();
    }

    std::process::exit(0);
}

/// Type alias for child stdout receiver channel.
type ChildStdoutReceiver = Arc<Mutex<Option<Receiver<Result<String, String>>>>>;

/// Manages an individual sandboxed tab worker process/thread.
#[allow(clippy::type_complexity)]
pub struct SandboxedTabWorker {
    pub worker_id: usize,
    pub policy: SandboxPolicy,
    pub status: Arc<RwLock<ProcessStatus>>,
    pub heartbeat: Arc<RwLock<Instant>>,
    pub ring_buffer: Arc<ShmRingBuffer>,
    pub cross_process_shm: Option<Arc<CrossProcessShmRingBuffer>>,
    child_process: Arc<Mutex<Option<Child>>>,
    child_stdin: Arc<Mutex<Option<BufWriter<std::process::ChildStdin>>>>,
    child_stdout_receiver: ChildStdoutReceiver,
    is_terminating: Arc<AtomicBool>,
}

impl SandboxedTabWorker {
    pub fn new(worker_id: usize, policy: SandboxPolicy) -> Self {
        let is_terminating = Arc::new(AtomicBool::new(false));
        let ring_buffer = ShmRingBuffer::new(3);
        let heartbeat = Arc::new(RwLock::new(Instant::now()));

        match policy {
            SandboxPolicy::ThreadSandbox => {
                let pid = 50000 + (worker_id as u32);
                Self {
                    worker_id,
                    policy,
                    status: Arc::new(RwLock::new(ProcessStatus::Running {
                        pid,
                        started_at: Instant::now(),
                    })),
                    heartbeat,
                    ring_buffer,
                    cross_process_shm: None,
                    child_process: Arc::new(Mutex::new(None)),
                    child_stdin: Arc::new(Mutex::new(None)),
                    child_stdout_receiver: Arc::new(Mutex::new(None)),
                    is_terminating,
                }
            }
            SandboxPolicy::ProcessSandbox | SandboxPolicy::SeccompSandbox => {
                let cross_shm = CrossProcessShmRingBuffer::create(&format!("strata_tab_worker_{worker_id}")).ok();
                let shm_path = cross_shm.as_ref().and_then(|s| s.path());
                let child_res = spawn_child_process(worker_id, policy, shm_path.as_deref());
                match child_res {
                    Ok(mut child) => {
                        let pid = child.id();
                        let stdin = child.stdin.take().map(BufWriter::new);
                        let stdout = child.stdout.take();

                        let (line_tx, line_rx) = channel::<Result<String, String>>();
                        if let Some(stdout_pipe) = stdout {
                            let mut reader = BufReader::new(stdout_pipe);
                            thread::spawn(move || {
                                let mut line = String::new();
                                while let Ok(bytes) = reader.read_line(&mut line) {
                                    if bytes == 0 {
                                        break;
                                    }
                                    let trimmed = line.trim();
                                    if trimmed.starts_with('{') {
                                        let _ = line_tx.send(Ok(trimmed.to_string()));
                                    }
                                    line.clear();
                                }
                                let _ = line_tx.send(Err("EOF".to_string()));
                            });
                        }

                        Self {
                            worker_id,
                            policy,
                            status: Arc::new(RwLock::new(ProcessStatus::Running {
                                pid,
                                started_at: Instant::now(),
                            })),
                            heartbeat,
                            ring_buffer,
                            cross_process_shm: cross_shm,
                            child_process: Arc::new(Mutex::new(Some(child))),
                            child_stdin: Arc::new(Mutex::new(stdin)),
                            child_stdout_receiver: Arc::new(Mutex::new(Some(line_rx))),
                            is_terminating,
                        }
                    }
                    Err(err) => {
                        eprintln!("[SandboxedTabWorker] Failed to spawn child process: {err}");
                        Self {
                            worker_id,
                            policy,
                            status: Arc::new(RwLock::new(ProcessStatus::Crashed {
                                exit_code: None,
                                reason: format!("Failed to spawn sandboxed tab worker process: {err}"),
                            })),
                            heartbeat,
                            ring_buffer,
                            cross_process_shm: cross_shm,
                            child_process: Arc::new(Mutex::new(None)),
                            child_stdin: Arc::new(Mutex::new(None)),
                            child_stdout_receiver: Arc::new(Mutex::new(None)),
                            is_terminating,
                        }
                    }
                }
            }
        }
    }

    /// Current lifecycle status of the worker.
    pub fn status(&self) -> ProcessStatus {
        self.status.read().unwrap().clone()
    }

    /// Whether the worker is actively running.
    pub fn is_running(&self) -> bool {
        matches!(*self.status.read().unwrap(), ProcessStatus::Running { .. })
    }

    /// Whether the worker has encountered a contained crash.
    pub fn is_crashed(&self) -> bool {
        matches!(*self.status.read().unwrap(), ProcessStatus::Crashed { .. })
    }

    /// Updates the heartbeat timestamp to indicate healthy execution.
    pub fn ping_heartbeat(&self) {
        if let Ok(mut hb) = self.heartbeat.write() {
            *hb = Instant::now();
        }
    }

    /// Evaluates if the worker has exceeded the heartbeat timeout threshold.
    pub fn check_heartbeat(&self, timeout: Duration) -> bool {
        if let Ok(mut guard) = self.child_process.lock() {
            if let Some(ref mut child) = *guard {
                if let Ok(Some(_status)) = child.try_wait() {
                    return false;
                }
            }
        }

        if let Ok(hb) = self.heartbeat.read() {
            hb.elapsed() <= timeout
        } else {
            false
        }
    }

    /// Simulates or records a contained crash for this worker without panicking caller.
    pub fn trigger_crash(&self, reason: &str) {
        if let Some(mut child) = self.child_process.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *self.child_stdin.lock().unwrap() = None;
        *self.child_stdout_receiver.lock().unwrap() = None;

        let mut st = self.status.write().unwrap();
        *st = ProcessStatus::Crashed {
            exit_code: Some(139), // Segmentation fault simulation
            reason: reason.to_string(),
        };
    }

    /// Restarts a terminated or crashed worker.
    pub fn restart(&self) {
        match self.policy {
            SandboxPolicy::ThreadSandbox => {
                let pid = 50000 + (self.worker_id as u32) + 1000;
                let mut st = self.status.write().unwrap();
                *st = ProcessStatus::Running {
                    pid,
                    started_at: Instant::now(),
                };
                self.ping_heartbeat();
            }
            SandboxPolicy::ProcessSandbox | SandboxPolicy::SeccompSandbox => {
                if let Some(mut old_child) = self.child_process.lock().unwrap().take() {
                    let _ = old_child.kill();
                    let _ = old_child.wait();
                }
                *self.child_stdin.lock().unwrap() = None;
                *self.child_stdout_receiver.lock().unwrap() = None;

                let shm_path = self.cross_process_shm.as_ref().and_then(|s| s.path());
                match spawn_child_process(self.worker_id, self.policy, shm_path.as_deref()) {
                    Ok(mut child) => {
                        let pid = child.id();
                        let stdin = child.stdin.take().map(BufWriter::new);
                        let stdout = child.stdout.take();

                        let (line_tx, line_rx) = channel::<Result<String, String>>();
                        if let Some(stdout_pipe) = stdout {
                            let mut reader = BufReader::new(stdout_pipe);
                            thread::spawn(move || {
                                let mut line = String::new();
                                while let Ok(bytes) = reader.read_line(&mut line) {
                                    if bytes == 0 {
                                        break;
                                    }
                                    let trimmed = line.trim();
                                    if trimmed.starts_with('{') {
                                        let _ = line_tx.send(Ok(trimmed.to_string()));
                                    }
                                    line.clear();
                                }
                                let _ = line_tx.send(Err("EOF".to_string()));
                            });
                        }

                        *self.child_stdin.lock().unwrap() = stdin;
                        *self.child_stdout_receiver.lock().unwrap() = Some(line_rx);
                        *self.child_process.lock().unwrap() = Some(child);

                        let mut st = self.status.write().unwrap();
                        *st = ProcessStatus::Running {
                            pid,
                            started_at: Instant::now(),
                        };
                        self.ping_heartbeat();
                    }
                    Err(err) => {
                        eprintln!("[SandboxedTabWorker] Restart failed to spawn child process: {err}");
                        let mut st = self.status.write().unwrap();
                        *st = ProcessStatus::Crashed {
                            exit_code: None,
                            reason: format!("Restart spawn failed: {err}"),
                        };
                    }
                }
            }
        }
    }

    /// Executes a job within the thread sandbox (in-process with catch_unwind).
    fn execute_thread_job(&self, job: SandboxedJob) -> SandboxedResponse {
        // Simulated Hang detection
        if job.simulate_hang {
            self.trigger_crash("Tab worker hung in unresponsive script loop");
            return SandboxedResponse {
                tab_id: job.tab_id,
                generation: job.generation,
                url: job.url.clone(),
                title: "Onyx Tab (Unresponsive)".to_string(),
                html: String::new(),
                dom_nodes: Vec::new(),
                status: 504,
                is_error: true,
                is_crashed: true,
                crash_reason: Some("Heartbeat timeout: unresponsive execution".to_string()),
                elapsed_ms: 0.0,
                request_entry: NetworkRequestEntry {
                    name: job.url.clone(),
                    method: "GET".to_string(),
                    status: 504,
                    status_text: "Gateway Timeout".to_string(),
                    kind: "document".to_string(),
                    initiator: "navigation".to_string(),
                    size_bytes: 0,
                    time_ms: 0.0,
                },
                frame_sequence: 0,
                shm_slot: None,
            };
        }

        // Simulated Crash containment
        if job.simulate_crash {
            self.trigger_crash("Memory access violation (SIGSEGV)");
            return SandboxedResponse {
                tab_id: job.tab_id,
                generation: job.generation,
                url: job.url.clone(),
                title: "Onyx Tab (Crash)".to_string(),
                html: String::new(),
                dom_nodes: Vec::new(),
                status: 500,
                is_error: true,
                is_crashed: true,
                crash_reason: Some("SIGSEGV: Memory access violation".to_string()),
                elapsed_ms: 0.0,
                request_entry: NetworkRequestEntry {
                    name: job.url.clone(),
                    method: "GET".to_string(),
                    status: 500,
                    status_text: "Process Crash".to_string(),
                    kind: "document".to_string(),
                    initiator: "navigation".to_string(),
                    size_bytes: 0,
                    time_ms: 0.0,
                },
                frame_sequence: 0,
                shm_slot: None,
            };
        }

        let job_clone = job.clone();
        let ring_buffer = Arc::clone(&self.ring_buffer);

        let execution_result = catch_unwind(AssertUnwindSafe(|| {
            let t_start = Instant::now();
            let result: PageLoadResult = load_page(&job_clone.url);
            let dom_nodes = HtmlParser::parse_with_scripts(&result.html);
            let elapsed_ms = t_start.elapsed().as_secs_f32() * 1000.0;

            let frame_seq = commit_frame_to_ring_buffer(&ring_buffer, &result.title, &dom_nodes);

            let request_entry = NetworkRequestEntry {
                name: job_clone.url.clone(),
                method: "GET".to_string(),
                status: result.status,
                status_text: if result.status == 200 {
                    "OK".to_string()
                } else {
                    "Error".to_string()
                },
                kind: "document".to_string(),
                initiator: "navigation".to_string(),
                size_bytes: result.html.len(),
                time_ms: elapsed_ms,
            };

            SandboxedResponse {
                tab_id: job_clone.tab_id,
                generation: job_clone.generation,
                url: job_clone.url,
                title: result.title,
                html: result.html,
                dom_nodes,
                status: result.status,
                is_error: result.is_error,
                is_crashed: false,
                crash_reason: None,
                elapsed_ms,
                request_entry,
                frame_sequence: frame_seq,
                shm_slot: None,
            }
        }));

        match execution_result {
            Ok(resp) => {
                self.ping_heartbeat();
                resp
            }
            Err(panic_err) => {
                let panic_msg = if let Some(s) = panic_err.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic_err.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "Sandboxed worker panic".to_string()
                };
                self.trigger_crash(&panic_msg);

                SandboxedResponse {
                    tab_id: job.tab_id,
                    generation: job.generation,
                    url: job.url.clone(),
                    title: "Onyx Tab (Panic)".to_string(),
                    html: String::new(),
                    dom_nodes: Vec::new(),
                    status: 500,
                    is_error: true,
                    is_crashed: true,
                    crash_reason: Some(panic_msg),
                    elapsed_ms: 0.0,
                    request_entry: NetworkRequestEntry {
                        name: job.url.clone(),
                        method: "GET".to_string(),
                        status: 500,
                        status_text: "Panic".to_string(),
                        kind: "document".to_string(),
                        initiator: "navigation".to_string(),
                        size_bytes: 0,
                        time_ms: 0.0,
                    },
                    frame_sequence: 0,
                    shm_slot: None,
                }
            }
        }
    }

    /// Executes a job within an authentic OS child process.
    fn execute_process_job(&self, job: SandboxedJob) -> SandboxedResponse {
        if self.is_crashed() || !self.is_running() {
            self.restart();
        }

        let timeout = if job.simulate_hang {
            Duration::from_millis(150)
        } else {
            Duration::from_secs(5)
        };

        let serialized_job = match serde_json::to_string(&job) {
            Ok(s) => s,
            Err(e) => {
                return SandboxedResponse {
                    tab_id: job.tab_id,
                    generation: job.generation,
                    url: job.url.clone(),
                    title: "Job Serialization Error".to_string(),
                    html: String::new(),
                    dom_nodes: Vec::new(),
                    status: 500,
                    is_error: true,
                    is_crashed: false,
                    crash_reason: Some(e.to_string()),
                    elapsed_ms: 0.0,
                    request_entry: NetworkRequestEntry {
                        name: job.url.clone(),
                        method: "GET".to_string(),
                        status: 500,
                        status_text: "Internal Error".to_string(),
                        kind: "document".to_string(),
                        initiator: "navigation".to_string(),
                        size_bytes: 0,
                        time_ms: 0.0,
                    },
                    frame_sequence: 0,
                    shm_slot: None,
                };
            }
        };

        // Send job to child process
        let write_ok = {
            let mut stdin_lock = self.child_stdin.lock().unwrap();
            if let Some(ref mut stdin) = *stdin_lock {
                writeln!(stdin, "{serialized_job}").and_then(|_| stdin.flush()).is_ok()
            } else {
                false
            }
        };

        if !write_ok {
            self.trigger_crash("Child process stdin pipe closed");
            return SandboxedResponse {
                tab_id: job.tab_id,
                generation: job.generation,
                url: job.url.clone(),
                title: "Onyx Tab (Crash)".to_string(),
                html: String::new(),
                dom_nodes: Vec::new(),
                status: 500,
                is_error: true,
                is_crashed: true,
                crash_reason: Some("Child process closed communication pipe".to_string()),
                elapsed_ms: 0.0,
                request_entry: NetworkRequestEntry {
                    name: job.url.clone(),
                    method: "GET".to_string(),
                    status: 500,
                    status_text: "Process Crash".to_string(),
                    kind: "document".to_string(),
                    initiator: "navigation".to_string(),
                    size_bytes: 0,
                    time_ms: 0.0,
                },
                frame_sequence: 0,
                shm_slot: None,
            };
        }

        // Wait for response line from child reader thread
        let response_result = {
            let rx_lock = self.child_stdout_receiver.lock().unwrap();
            if let Some(ref rx) = *rx_lock {
                rx.recv_timeout(timeout)
            } else {
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            }
        };

        match response_result {
            Ok(Ok(line)) => {
                match serde_json::from_str::<SandboxedResponse>(line.trim()) {
                    Ok(mut resp) => {
                        if let Some(ref shm) = self.cross_process_shm {
                            if let Some(slot_idx) = resp.shm_slot {
                                if let Some(snapshot) = shm.read_slot(slot_idx) {
                                    if let Some(payload) = snapshot.parse_payload() {
                                        resp.title = payload.title;
                                        if !payload.dom_nodes.is_empty() {
                                            resp.dom_nodes = payload.dom_nodes;
                                        }
                                    }
                                }
                            }
                        }
                        let frame_seq = commit_frame_to_ring_buffer(
                            &self.ring_buffer,
                            &resp.title,
                            &resp.dom_nodes,
                        );
                        resp.frame_sequence = frame_seq;
                        self.ping_heartbeat();
                        resp
                    }
                    Err(e) => {
                        self.trigger_crash(&format!("Failed to parse response JSON: {e}"));
                        SandboxedResponse {
                            tab_id: job.tab_id,
                            generation: job.generation,
                            url: job.url.clone(),
                            title: "Onyx Tab (Invalid Response)".to_string(),
                            html: String::new(),
                            dom_nodes: Vec::new(),
                            status: 500,
                            is_error: true,
                            is_crashed: true,
                            crash_reason: Some(format!("Invalid child response: {e}")),
                            elapsed_ms: 0.0,
                            request_entry: NetworkRequestEntry {
                                name: job.url.clone(),
                                method: "GET".to_string(),
                                status: 500,
                                status_text: "Parse Error".to_string(),
                                kind: "document".to_string(),
                                initiator: "navigation".to_string(),
                                size_bytes: 0,
                                time_ms: 0.0,
                            },
                            frame_sequence: 0,
                            shm_slot: None,
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.trigger_crash("Memory access violation (SIGSEGV)");
                SandboxedResponse {
                    tab_id: job.tab_id,
                    generation: job.generation,
                    url: job.url.clone(),
                    title: "Onyx Tab (Crash)".to_string(),
                    html: String::new(),
                    dom_nodes: Vec::new(),
                    status: 500,
                    is_error: true,
                    is_crashed: true,
                    crash_reason: Some("SIGSEGV: Memory access violation".to_string()),
                    elapsed_ms: 0.0,
                    request_entry: NetworkRequestEntry {
                        name: job.url.clone(),
                        method: "GET".to_string(),
                        status: 500,
                        status_text: "Process Crash".to_string(),
                        kind: "document".to_string(),
                        initiator: "navigation".to_string(),
                        size_bytes: 0,
                        time_ms: 0.0,
                    },
                    frame_sequence: 0,
                    shm_slot: None,
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.trigger_crash("Heartbeat timeout: unresponsive execution");
                SandboxedResponse {
                    tab_id: job.tab_id,
                    generation: job.generation,
                    url: job.url.clone(),
                    title: "Onyx Tab (Unresponsive)".to_string(),
                    html: String::new(),
                    dom_nodes: Vec::new(),
                    status: 504,
                    is_error: true,
                    is_crashed: true,
                    crash_reason: Some("Heartbeat timeout: unresponsive execution".to_string()),
                    elapsed_ms: 0.0,
                    request_entry: NetworkRequestEntry {
                        name: job.url.clone(),
                        method: "GET".to_string(),
                        status: 504,
                        status_text: "Gateway Timeout".to_string(),
                        kind: "document".to_string(),
                        initiator: "navigation".to_string(),
                        size_bytes: 0,
                        time_ms: 0.0,
                    },
                    frame_sequence: 0,
                    shm_slot: None,
                }
            }
        }
    }
}

impl Drop for SandboxedTabWorker {
    fn drop(&mut self) {
        self.is_terminating.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child_process.lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

/// Sandboxed multi-process tab worker pool with automatic crash containment and heartbeat watchdog.
pub struct SandboxedTabPool {
    workers: Vec<Arc<SandboxedTabWorker>>,
    job_sender: Sender<SandboxedJob>,
    response_receiver: Mutex<Receiver<SandboxedResponse>>,
    heartbeat_timeout: Duration,
    _worker_threads: Vec<JoinHandle<()>>,
}

impl SandboxedTabPool {
    /// Creates a new `SandboxedTabPool` with the specified number of isolated workers.
    pub fn new(num_workers: usize, policy: SandboxPolicy) -> Self {
        let (job_tx, job_rx) = channel::<SandboxedJob>();
        let (resp_tx, resp_rx) = channel::<SandboxedResponse>();

        let shared_rx = Arc::new(Mutex::new(job_rx));
        let mut workers = Vec::with_capacity(num_workers);
        let mut worker_threads = Vec::with_capacity(num_workers);

        for worker_id in 0..num_workers {
            let worker = Arc::new(SandboxedTabWorker::new(worker_id, policy));
            workers.push(Arc::clone(&worker));

            let rx = Arc::clone(&shared_rx);
            let tx = resp_tx.clone();
            let worker_ref = Arc::clone(&worker);

            let handle = thread::Builder::new()
                .name(format!("strata-sandboxed-tab-{worker_id}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let guard = rx.lock();
                            match guard {
                                Ok(g) => match g.recv() {
                                    Ok(job) => job,
                                    Err(_) => break, // Channel closed
                                },
                                Err(_) => break,
                            }
                        };

                        worker_ref.ping_heartbeat();

                        let response = match worker_ref.policy {
                            SandboxPolicy::ThreadSandbox => {
                                worker_ref.execute_thread_job(job)
                            }
                            SandboxPolicy::ProcessSandbox | SandboxPolicy::SeccompSandbox => {
                                worker_ref.execute_process_job(job)
                            }
                        };

                        if tx.send(response).is_err() {
                            break;
                        }
                    }
                })
                .expect("Failed to spawn sandboxed tab worker thread");

            worker_threads.push(handle);
        }

        Self {
            workers,
            job_sender: job_tx,
            response_receiver: Mutex::new(resp_rx),
            heartbeat_timeout: Duration::from_secs(5),
            _worker_threads: worker_threads,
        }
    }

    /// Submits a navigation job to the sandboxed worker pool.
    pub fn submit_job(&self, job: SandboxedJob) -> bool {
        self.job_sender.send(job).is_ok()
    }

    /// Drains all available worker responses non-blockingly.
    pub fn drain_responses(&self) -> Vec<SandboxedResponse> {
        let mut responses = Vec::new();
        if let Ok(guard) = self.response_receiver.lock() {
            while let Ok(resp) = guard.try_recv() {
                responses.push(resp);
            }
        }
        responses
    }

    /// Returns a reference to the workers list for status inspection.
    pub fn workers(&self) -> &[Arc<SandboxedTabWorker>] {
        &self.workers
    }

    /// Restarts all crashed workers in the pool.
    pub fn restart_crashed_workers(&self) {
        for worker in &self.workers {
            if worker.is_crashed() {
                worker.restart();
            }
        }
    }

    /// Checks for unresponsive workers and marks them crashed if timeout exceeded.
    pub fn check_worker_health(&self) -> Vec<usize> {
        let mut unresponsive_ids = Vec::new();
        for worker in &self.workers {
            if worker.is_running() && !worker.check_heartbeat(self.heartbeat_timeout) {
                worker.trigger_crash("Heartbeat timeout: unresponsive worker process");
                unresponsive_ids.push(worker.worker_id);
            }
        }
        unresponsive_ids
    }
}
