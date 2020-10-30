use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{EpollAttr, EpollCtlCmd, EpollEvent, EpollFlags};
use crate::fs::{HostFd, HostFile, IoEvents};
use crate::prelude::*;

// TODO: the `add/mod/del_file` operation can be postponed until a `poll_files` operation
// to reduce the number of OCalls.

/// An epoll-based helper type to update the states of a set of host files.
#[derive(Debug)]
pub struct HostFileEpoller {
    host_files: SgxMutex<HashMap<u32, Arc<dyn HostFile>>>,
    host_fd: HostFd,
    is_ready: AtomicBool,
}

impl HostFileEpoller {
    pub fn new() -> Self {
        let host_files = Default::default();
        let host_fd = {
            let raw_host_fd = (|| -> Result<u32> {
                let raw_host_fd = try_libc!(libc::ocall::epoll_create1(0)) as u32;
                Ok(raw_host_fd)
            })()
            .expect("epoll_create should never fail");

            HostFd::new(raw_host_fd)
        };
        let is_ready = Default::default();
        Self {
            host_files,
            host_fd,
            is_ready,
        }
    }

    pub fn host_fd(&self) -> &HostFd {
        &self.host_fd
    }

    pub fn poll(&self) -> IoEvents {
        if self.is_ready.load(Ordering::Acquire) {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }

    pub fn recv_host_events(&self, events: &IoEvents) {
        let is_ready = events.contains(IoEvents::IN);
        self.is_ready.store(is_ready, Ordering::Release);
    }

    pub fn add_file(&self, host_file: Arc<dyn HostFile>, ep_attr: &EpollAttr) -> Result<()> {
        let mut host_files = self.host_files.lock().unwrap();
        let host_fd = host_file.host_fd().to_u32();
        let already_added = host_files.insert(host_fd, host_file.clone()).is_some();
        if already_added {
            // TODO: handle the case where one `HostFile` is somehow to be added more than once.
            warn!(
                "Cannot handle the case of adding the same `HostFile` twice in a robust way.
                This can happen if the same `HostFile` is accessible via two different LibOS fds."
            );
            return Ok(());
        }

        self.do_epoll_ctl(EpollCtlCmd::Add, &host_file, Some(ep_attr))
    }

    pub fn mod_file(&self, host_file: &Arc<dyn HostFile>, ep_attr: &EpollAttr) -> Result<()> {
        let host_files = self.host_files.lock().unwrap();
        let host_fd = host_file.host_fd().to_u32();
        let not_added = !host_files.contains_key(&host_fd);
        if not_added {
            return_errno!(ENOENT, "the host file must be added before modifying");
        }

        self.do_epoll_ctl(EpollCtlCmd::Mod, &host_file, Some(ep_attr))
    }

    pub fn del_file(&self, host_file: &Arc<dyn HostFile>) -> Result<()> {
        let mut host_files = self.host_files.lock().unwrap();
        let host_fd = host_file.host_fd().to_u32();
        let not_added = !host_files.remove(&host_fd).is_some();
        if not_added {
            return_errno!(ENOENT, "the host file must be added before deleting");
        }

        self.do_epoll_ctl(EpollCtlCmd::Del, &host_file, None)
    }

    fn do_epoll_ctl(
        &self,
        cmd: EpollCtlCmd,
        host_file: &Arc<dyn HostFile>,
        ep_attr: Option<&EpollAttr>,
    ) -> Result<()> {
        let mut c_event = ep_attr.map(|ep_attr| ep_attr.to_c());
        if let Some(c_event) = c_event.as_mut() {
            c_event.u64 = host_file.host_fd().to_u32() as u64;
        }

        try_libc!(libc::ocall::epoll_ctl(
            self.host_fd.to_u32() as i32,
            cmd as i32,
            host_file.host_fd().to_u32() as i32,
            c_event.as_mut().map_or(ptr::null_mut(), |c_event| c_event),
        ));
        Ok(())
    }

    pub fn poll_files(&self, max_count: usize) -> usize {
        let mut raw_events = vec![MaybeUninit::<libc::epoll_event>::uninit(); max_count];
        let timeout = 0;
        let ocall_res = || -> Result<usize> {
            let count = try_libc!(libc::ocall::epoll_wait(
                self.host_fd.to_u32() as i32,
                raw_events.as_mut_ptr() as *mut _,
                raw_events.len() as c_int,
                timeout,
            )) as usize;
            assert!(count <= max_count);
            Ok(count)
        }();

        let count = match ocall_res {
            Err(e) => {
                warn!("unexpected error from ocall::epoll_wait(): {:?}", e);
                0
            }
            Ok(count) => count,
        };

        if count == 0 {
            return 0;
        }

        let mut host_files = self.host_files.lock().unwrap();
        for raw_event in &raw_events[..count] {
            let raw_event = unsafe { raw_event.assume_init() };
            let io_events = IoEvents::from_bits_truncate(raw_event.events as u32);
            let host_fd = raw_event.u64 as u32;

            let host_file = match host_files.get(&host_fd) {
                None => continue,
                Some(host_file) => host_file,
            };

            host_file.recv_host_events(&io_events);
        }
        count
    }
}
