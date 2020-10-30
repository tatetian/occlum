use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use super::*;

pub trait HostFile: File {
    /// Return the host fd.
    ///
    /// Note that the `HostFd` type guarantees that each instance of `HostFile`
    /// has a different value of `HostFd`.
    fn host_fd(&self) -> &HostFd;

    /// Receive events from the host.
    ///
    /// After calling this method, the `poll` method of the `File` trait will
    /// return the latest events on the `HostFile`.
    fn recv_host_events(&self, events: &IoEvents);
}

/// A unique fd from the host OS.
///
/// The uniqueness property is important both
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct HostFd(u32);

impl HostFd {
    pub fn new(host_fd: u32) -> Self {
        HOST_FD_REGISTRY.lock().unwrap().register(host_fd).unwrap();
        Self(host_fd)
    }

    pub fn to_u32(&self) -> u32 {
        self.0
    }
}

impl Drop for HostFd {
    fn drop(&mut self) {
        HOST_FD_REGISTRY
            .lock()
            .unwrap()
            .unregister(self.to_u32())
            .unwrap();
    }
}

/// A registery for host fds to ensure that they are unique.
struct HostFdRegistry {
    set: HashSet<u32>,
}

impl HostFdRegistry {
    pub fn new() -> Self {
        Self {
            set: HashSet::new(),
        }
    }

    pub fn register(&mut self, host_fd: u32) -> Result<()> {
        let existed = self.set.insert(host_fd);
        if existed {
            return_errno!(EEXIST, "host fd has been registered");
        }
        Ok(())
    }

    pub fn unregister(&mut self, host_fd: u32) -> Result<()> {
        let existed = self.set.remove(&host_fd);
        if !existed {
            return_errno!(ENOENT, "host fd has NOT been registered");
        }
        Ok(())
    }
}

lazy_static! {
    static ref HOST_FD_REGISTRY: SgxMutex<HostFdRegistry> =
        { SgxMutex::new(HostFdRegistry::new()) };
}
