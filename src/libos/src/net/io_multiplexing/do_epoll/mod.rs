use std::convert::TryFrom;

use crate::fs::IoEvents;
use crate::prelude::*;

mod epoll_file;
mod host_file_epoller;

#[derive(Copy, Clone, PartialEq)]
pub enum EpollCtlCmd {
    /// Add a file decriptor to the interface
    Add = 1,
    /// Remove a file decriptor from the interface
    Del = 2,
    /// Change file decriptor epoll_event structre
    Mod = 3,
}

impl TryFrom<i32> for EpollCtlCmd {
    type Error = Error;

    fn try_from(op_num: i32) -> Result<Self> {
        match op_num {
            1 => Ok(EpollCtlCmd::Add),
            2 => Ok(EpollCtlCmd::Del),
            3 => Ok(EpollCtlCmd::Mod),
            _ => return_errno!(EINVAL, "invalid operation number"),
        }
    }
}

bitflags! {
    pub struct EpollFlags: u32 {
        const EXCLUSIVE      = (1 << 12);
        const WAKE_UP        = (1 << 13);
        const ONE_SHOT       = (1 << 14);
        const EDGE_TRIGGER   = (1 << 15);
    }
}

// Note: the memory layout is compatible with that of C's struct epoll_event.
#[derive(Debug, Copy, Clone)]
pub struct EpollEvent {
    mask: IoEvents,
    user_data: u64,
}

impl EpollEvent {
    pub fn new(mask: IoEvents, user_data: u64) -> Self {
        Self { mask, user_data }
    }

    pub fn mask(&self) -> IoEvents {
        self.mask
    }

    pub fn user_data(&self) -> u64 {
        self.user_data
    }

    pub fn from_c(c_event: &libc::epoll_event) -> Self {
        let mask = IoEvents::from_bits_truncate(c_event.events);
        let user_data = c_event.u64;
        Self { mask, user_data }
    }

    pub fn to_c(&self) -> libc::epoll_event {
        libc::epoll_event {
            events: self.mask.bits(),
            u64: self.user_data,
        }
    }
}

/// Epoll attributes for epoll commands.
#[derive(Debug, Copy, Clone)]
pub struct EpollAttr {
    event: EpollEvent,
    flags: EpollFlags,
}

impl EpollAttr {
    pub fn new(mut event: EpollEvent, flags: EpollFlags) -> Self {
        Self { event, flags }.add_default_events()
    }

    fn add_default_events(mut self) -> Self {
        // Add two events that are reported by default
        self.event.mask |= (IoEvents::ERR | IoEvents::HUP);
        self
    }

    pub fn event(&self) -> &EpollEvent {
        &self.event
    }

    pub fn flags(&self) -> EpollFlags {
        self.flags
    }

    pub fn from_c(c_event: &libc::epoll_event) -> Self {
        let event = EpollEvent::from_c(c_event);
        let flags = EpollFlags::from_bits_truncate(c_event.events);
        Self { event, flags }.add_default_events()
    }

    pub fn to_c(&self) -> libc::epoll_event {
        let mut c_event = self.event.to_c();
        c_event.events |= self.flags.bits();
        c_event
    }
}
