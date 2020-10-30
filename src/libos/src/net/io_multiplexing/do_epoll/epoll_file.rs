use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::mem::{self, MaybeUninit};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Weak;
use std::time::Duration;

use super::host_file_epoller::HostFileEpoller;
use super::{EpollAttr, EpollCtlCmd, EpollEvent, EpollFlags};
use crate::events::{Observer, Waiter, WaiterQueue};
use crate::fs::{File, HostFile, IoEvents, IoNotifier};
use crate::prelude::*;

// TODO: handling closing file gracefully
// TODO: there is possible for two EpollFile to monitor each other?

/// A file that provides epoll API.
pub struct EpollFile {
    /// All interesting entries.
    interest: SgxMutex<HashMap<FileDesc, Arc<EpollEntry>>>,
    /// Entries that are probably ready (having events happened)
    ready: SgxMutex<VecDeque<Arc<EpollEntry>>>,
    waiters: WaiterQueue,
    notifier: IoNotifier,
    host_file_epoller: HostFileEpoller,
    weak_self: Weak<Self>,
}

impl fmt::Debug for EpollFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f
            .debug_struct("EpollFile")
            .field("interest", &self.interest.lock().unwrap())
            .field("ready", &self.ready.lock().unwrap())
            .finish()
    }
}

/*
impl EpollFile {
    pub fn new() -> Arc<Self> {
        let interest = Default::default();
        let ready = Default::default();
        let waiters = Default::default();
        let notifier = Default::default();
        let weak_self = Default::default();

        Self {
            interest,
            ready,
            waiters,
            notifier,
            weak_self,
        }.wrap_self()
    }

    fn wrap_self(self) -> Arc<Self> {
        let strong_self = Arc::new(self);
        let weak_self = Arc::downgrade(&strong_self);

        let ptr_self = Arc::into_raw(strong_self) as *mut Self;
        unsafe {
            *ptr_self.weak_self = weak_self;
        }
        let strong_self = Arc::from_raw(ptr_self);

        strong_self
    }

    pub fn ctl(
        self: &Arc<Self>,
        ep_op: EpollCtlCmd,
        fd: FileDesc,
        ep_attr: Option<&EpollAttr>,
    ) -> Result<()> {
        match ep_op {
            EpollCtlCmd::Add => {
                let ep_attr = ep_attr
                    .ok_or_else(|| errno!(EINVAL, "the ep_attr argument is needed for the add cmd"))?;

                self.add_interest(fd, *ep_attr)?;
            },
            EpollCtlCmd::Del => {
                self.del_interest(fd)?;
            },
            EpollCtlCmd::Mod => {
                let ep_attr = ep_attr
                    .ok_or_else(|| errno!(EINVAL, "the ep_attr argument is needed for the del cmd"))?;

                self.mod_interest(fd, *ep_attr)?;
            }
        }
        Ok(())
    }

    pub fn wait(
        self: &Arc<Self>,
        events: &mut [MaybeUninit<EpollEvent>],
        timeout: Option<&Duration>
    ) -> Result<usize> {
        let mut timeout = timeout.to_owned();

        let max_count = events.len();
        let mut reinsert = VecDeque::with_capacity(max_count);

        let waiter = Waiter::new();
        unsafe {
            waiter.add_host_events(self.host_file_epoller.host_fd(), IoEvents::In);
        }

        loop {
            self.waiters.reset_and_enqueue(&waiter);

            // Poll the latest states of the interested host files
            self.host_file_epoller.poll_from_host(max_count);

               // Poll the ready list to find as many results as possible
            let mut count = 0;
            while count < max_count {
                // Pop some entries from the ready list
                let mut ready_entries = self.pop_ready(max_count - count);

                // Note that while iterating the ready entries, we do not hold the lock
                // of the ready list. This reduces the chances of lock contention.
                for ep_entry in ready_entries.into_iter() {
                    if ep_entry.is_deleted.load(Ordering::Acquire) {
                        continue;
                    }

                    let mut attr = ep_entry.attr.lock().unwrap();

                    let mask = attr.event().mask();
                    let file = &ep_entry.file;
                    let events = file.poll() & mask;
                    if events.is_empty() {
                        continue;
                    }

                    let mut revent = attr.event;
                    revent.mask = events;
                    events[count].write(revent);

                    if !attr.flags().intersects(EpollFlags::EdgeTrigger | EpollFlags::OneShot) {
                        reinsert.push_back(ep_entry);
                    }

                    if attr.flags().contains(EpollFlags::OneShot) {
                        attr.event.mask = IoEvents::empty();
                    }

                    count += 1;
                }
            }

            // If any results, we can return
            if count > 0 {
                // Push the entries that are still ready after polling back to the ready list
                if reinsert.len() > 0 {
                    self.push_ready_iter(reinsert.into());
                }

                return Ok(count);
            }

            // If no results, try again later
            waiter.wait_mut(timeout.as_mut())?;
        }
    }

    fn mark_ready(&self) {
        self.notifier.broadcast(IoEvents::In);
        self.waiters.dequeue_and_wake_all();
    }

    fn push_ready(&self, ep_entry: Arc<EpollEntry>) {
        // Fast path
        if ep_entry.is_ready.load(Ordering::Releaxed) {
            return;
        }

        self.push_ready_iter(std::iter::once(ep_entry));
    }

    // Property. An instance of Arc<EpollEntry> appears in the ready list at most once.
    // Property. An instance of
    fn push_ready_iter<I: Iterator<Item = Arc<EpollEntry>>>(&self, ep_entries: I) {
        let mut has_pushed_any = false;

        // A critical section protected by self.ready.lock()
        {
            let mut ready_entries = self.ready.lock().unwrap();
            for ep_entry in ep_entries {
                if ep_entry.is_ready.load(Ordering::Releaxed) {
                    continue;
                }

                ep_entry.is_ready.store(true, Ordering::Releaxed);
                ready_entries.push_back(ep_entry);

                has_pushed_any = true;
            }
        }

        if has_pushed_any {
            self.mark_ready();
        }
    }

    fn pop_ready(&self, max_count: usize) -> VecDeque<Arc<EpollEntry>> {
        // A critical section protected by self.ready.lock()
        {
            let mut ready_entries = self.ready.lock().unwrap();
            let max_count = max_count.min(ready_entries.len());
            ready_entries
                .drain(..max_count)
                .map(|ep_entry| ep_entry.is_ready.store(false, Ordering::Relaxed))
                .collect::<VecDeque<EpollEntry>>()
        }
    }

    fn add_interest(self: &Arc<Self>, fd: FileDesc, ep_attr: EpollAttr) -> Result<()> {
        let file = current!().get(fd)?;
        if Arc::ptr_eq(self, &file) {
            return_errno!(EINVAL, "a epoll file cannot epoll itself");
        }

        if ep_attr.flags().intersects(EpollFlags::Enclusive | EpollFlags::WakeUp) {
            warn!("{:?} contains unsupported flags", ep_attr.flags());
        }

        let ep_entry = Arc::new(EpollEntry::new(fd, file, ep_attr));

        // A critical section protected by the lock of self.interest
        {
            let mut interest_entries = self.interest.lock().unwrap();
            if interest_entries.get(fd).is_some() {
                return_errno!(EEXIST, "fd is already registered");
            }
            interest_entries.insert(fd, ep_entry.clone());

            if let Some(host_file) = file.downcast_ref::<Arc<dyn HostFile>>() {
                self.host_file_epoller.add_file(host_file.clone(), &ep_attr);
            } else {
                // A LibOS file supports epoll if and only if it is attached with an IoNotifier
                let notifier = file.notifier()
                    .ok_or_else(|| errno!(EINVAL, "the fd does not support epoll"))?;
                // Start observing events on the target file.
                notifier.register(Arc::downgrade(self), Some(IoEvents::all()), Some(Arc::downgrade(&ep_entry)));
            }
        }

       self.push_ready(ep_entry);

       Ok(())
    }

    fn del_interest(self: &Arc<Self>, fd: FileDesc) -> Result<()> {
        // A critical section protected by the lock of self.interest
        {
            let mut interest_entries = self.interest.lock().unwrap();
            let ep_entry = interest_entries.remove(&fd)
                .ok_or_else(|| errno!(ENOENT, "fd is not added"))?;
            ep_entry.is_deleted.store(true, Ordering::Release);

            if let Some(host_file) = ep_entry.file.downcast_ref::<Arc<dyn HostFile>>() {
                self.host_file_epoller.del_file(host_file);
            } else {
                let notifier = ep_entry.file.notifier().unwrap();
                notifier.unregister(Arc::downgrade(self));
            }
        }
    }

    fn mod_interest(self: &Arc<Self>, fd: FileDesc, new_ep_attr: EpollAttr) {
        if new_ep_attr.flags().intersects(EpollFlags::Enclusive | EpollFlags::WakeUp) {
            warn!("{:?} contains unsupported flags", new_ep_attr.flags());
        }

        // A critical section protected by the lock of self.interest
        let ep_entry = {
            let mut interest_entries = self.interest.lock().unwrap();
            let ep_entry = interest_entries.get(&fd)
                .ok_or_else(|| errno!(ENOENT, "fd is not added"))?
                .clone();

            let mut old_ep_attr = ep_entry.attr.lock().unwrap();
            *old_ep_attr = new_ep_attr;
            drop(old_ep_attr);

            if let Some(host_file) = ep_entry.file.downcast_ref::<Arc<dyn HostFile>>() {
                self.host_file_epoller.mod_file(host_file, new_ep_attr);
            }

            ep_entry
        };

        self.push_ready(ep_entry);

        // TODO: a more thorough concurrency analysis is needed to rule out any chances of
        // losing interesting events whiling modifying the attributes of an existing `EpollEntry`.
    }
}

impl File for EpollFile {
    fn poll(&self) -> IoEvents {
        if !self.host_file_epoller.poll().is_empty() {
            return IoEvents::IN;
        }

        let ready_entries = self.ready.lock().unwrap();
        if !ready_entries.is_empty() {
            return IoEvents::IN
        }

        IoEvents::empty()
    }

    fn notifier(&self) -> Option<&IoNotifier> {
        Some(&self.notifier)
    }
}

impl HostFile for EpollFile {
    fn host_fd(&self) -> FileDesc {
        self.host_file_epoller.host_file()
    }

    fn recv_host_events(&self, events: &IoEvents) {
        self.host_file_epoller.recv_host_events(events);
    }
}

impl Drop for EpollFile {
    fn drop(&mut self) {
        // Do not try to `self.weak_self.upgrade()`! The Arc object must have been
        // dropped at this point.
        let mut interest_entries = self.interest.lock().unwrap();
        interest_entries
            .drain(interest_entries.len())
            .foreach(|ep_entry| {
                let notifier = ep_entry.file.notifier().unwrap();
                notifier.unregister(&self.weak_self);
            });
    }
}

impl Observer<IoEvents> for EpollFile {
    fn on_event(&self, _events: &IoEvents, metadata: Option<&Weak<Any>>) {
        let ep_entry = {
            let ep_entry_weak = metadata
                .unwrap()
                .downcast_ref::<&Weak<EpollEntry>>()
                .unwrap();
            match ep_entry_weak.upgrade() {
                None => return,
                Some(ep_entry) => ep_entry,
            }
        };

        self.push_ready(ep_entry);
    }
}*/

#[derive(Debug)]
struct EpollEntry {
    fd: FileDesc,
    file: FileRef,
    attr: SgxMutex<EpollAttr>,
    // Whether the entry is in the ready list
    is_ready: AtomicBool,
    // Whether the entry has been deleted from the interest list
    is_deleted: AtomicBool,
}

impl EpollEntry {
    pub fn new(fd: FileDesc, file: FileRef, attr: EpollAttr) -> Self {
        let is_ready = Default::default();
        let is_deleted = Default::default();
        let attr = SgxMutex::new(attr);
        Self {
            fd,
            file,
            attr,
            is_ready,
            is_deleted,
        }
    }
}
