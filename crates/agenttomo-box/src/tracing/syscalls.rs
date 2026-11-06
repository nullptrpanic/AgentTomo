//! Stop pairing and dispatch. Domain modules own their interpretation/state.

use crate::{
    collectors::{file, memory, network},
    network::association::Registry,
    syscall::{Call, Stop, query},
};
use nix::{sys::ptrace, unistd::Pid};
use std::{collections::HashMap, io, path::PathBuf};

pub struct Dispatch {
    pending: HashMap<Pid, Pending>,
    network: network::Collector,
    capture_files: bool,
}

enum Pending {
    File(file::Attempt),
    Memory(memory::Attempt),
    Network(Box<network::Attempt>),
    Query(Box<(query::Query, network::Query)>),
}

impl Dispatch {
    pub fn new(registry: Registry, netns: PathBuf, capture_files: bool) -> Self {
        Self {
            pending: HashMap::new(),
            network: network::Collector::new(registry, netns),
            capture_files,
        }
    }

    pub fn enter(&mut self, tid: Pid, process: Option<crate::process::Identity>) -> io::Result<()> {
        let call = Call::read(tid, process)?;
        if let Some(attempt) = file::enter(&call) {
            self.pending.insert(tid, Pending::File(attempt));
        } else if let Some(attempt) = memory::enter(&call) {
            self.pending.insert(tid, Pending::Memory(attempt));
        } else if let Some(attempt) = self.network.enter(call) {
            self.pending
                .insert(tid, Pending::Network(Box::new(attempt)));
        }
        Ok(())
    }

    pub fn exit(&mut self, tid: Pid) -> io::Result<Option<super::Event>> {
        let stop = Stop::read(tid)?;
        if stop.op != 2 {
            return Ok(None);
        }
        match self.pending.remove(&tid) {
            Some(Pending::File(attempt)) => Ok(Some(super::Event::FileOpen(
                attempt.finish(stop.result(), self.capture_files),
            ))),
            Some(Pending::Memory(attempt)) => Ok(Some(super::Event::Process {
                kind: "memory.mmap",
                data: attempt.finish(stop.result()),
            })),
            Some(Pending::Network(attempt)) => {
                if let Some(request) = self.network.finish(*attempt, stop.result())
                    && let Some(query) = query::Query::start(&request.call)?
                {
                    self.pending
                        .insert(tid, Pending::Query(Box::new((query, request))));
                }
                Ok(None)
            }
            Some(Pending::Query(query)) => {
                let (query, request) = *query;
                let source = query.finish(tid, &stop)?;
                self.network.queried(request, source);
                Ok(None)
            }
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
