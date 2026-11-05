//! Stop pairing and dispatch. Domain modules own their interpretation/state.

use crate::{
    collectors::{file, network},
    proxy,
    syscall::{Call, Stop},
};
use nix::{sys::ptrace, unistd::Pid};
use std::{collections::HashMap, io};

pub struct Dispatch {
    pending: HashMap<Pid, Pending>,
    network: network::Collector,
}

enum Pending {
    File(file::Attempt),
    Network(network::Attempt),
}

impl Dispatch {
    pub fn new(proxy: proxy::Handle) -> Self {
        Self {
            pending: HashMap::new(),
            network: network::Collector::new(proxy),
        }
    }

    pub fn enter(&mut self, tid: Pid, task_id: u64) -> io::Result<()> {
        let call = Call::read(tid, task_id)?;
        if let Some(attempt) = file::enter(&call) {
            self.pending.insert(tid, Pending::File(attempt));
        } else if let Some(attempt) = self.network.enter(&call)? {
            self.pending.insert(tid, Pending::Network(attempt));
        }
        Ok(())
    }

    pub fn exit(&mut self, tid: Pid) -> io::Result<Option<(&'static str, serde_json::Value)>> {
        let stop = Stop::read(tid)?;
        if stop.op != 2 {
            return Ok(None);
        }
        match self.pending.remove(&tid) {
            Some(Pending::File(attempt)) => Ok(Some(("file.open", attempt.finish(stop.result())))),
            Some(Pending::Network(attempt)) => Ok(self
                .network
                .finish(attempt, stop.result())?
                .map(|data| ("network.connect", data))),
            None => Ok(None),
        }
    }

    pub fn forget(&mut self, tid: Pid) {
        self.pending.remove(&tid);
    }

    pub fn resume(&self, tid: Pid, signal: Option<nix::sys::signal::Signal>) -> nix::Result<()> {
        if self.pending.contains_key(&tid) {
            ptrace::syscall(tid, signal)
        } else {
            ptrace::cont(tid, signal)
        }
    }
}
