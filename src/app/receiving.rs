//! Receiving files from someone else's share.

use crate::*;

impl App {
    pub(crate) fn start_recv(&mut self, input: &str) {
        self.stop_recv();
        self.recv.error.clear();
        self.recv.log = Log::default();
        self.recv.link = Link::default();
        self.recv.host_name = None;
        let c = match code::parse(input) {
            Ok(c) if c.kind == Kind::File => c,
            Ok(c) if c.kind == Kind::Port => return self.fail_recv("That's a port code. Paste it in the Port Forward tab.".into()),
            Ok(c) if c.kind == Kind::Game => {
                return self.fail_recv("That's a game server code. Paste it in Port Forward, under Game servers.".into())
            }
            Ok(c) if c.kind == Kind::Mount => return self.fail_recv("That's a mount code. Paste it in the Mounts tab.".into()),
            Ok(c) if c.kind == Kind::Voip => return self.fail_recv("That's a room code. Paste it in the VOIP tab.".into()),
            Ok(c) if c.kind == Kind::Hub => return self.fail_recv(HUB_CODE_HERE.into()),
            Ok(_) => return self.fail_recv("That's a port code. Paste it in the Port Forward tab, or ask for the file code (it starts with kitty:file:).".into()),
            Err(e) => return self.fail_recv(e),
        };
        let local = match free_port() {
            Ok(p) => p,
            Err(e) => return self.fail_recv(e),
        };
        let args: Vec<String> = vec!["forward".into(), c.addr.clone(), format!("{local}:{}", c.ports[0])];
        self.recv.log.push(format!("Portside: tunnelling 127.0.0.1:{local} to the sharer"));
        match tailcat::spawn(Role::RecvForward, &args, &self.proxy) {
            Ok(p) => {
                let cancel = Arc::new(AtomicBool::new(false));
                self.recv.proc = Some(p);
                self.recv.port = local;
                self.recv.args = args;
                self.recv.peer = peers::new_id();
                self.recv.addr = c.addr.clone();
                self.recv.cancel = cancel.clone();
                self.recv.jobs = Some(share::downloader(local, cancel.clone(), self.proxy.clone()));
                self.recv.st = St::Starting;
                share::fetch_list(local, cancel, self.proxy.clone());
                self.set_status("Connecting through TailCat… the first connection can take a few seconds", "busy");
            }
            Err(e) => self.fail_recv(e),
        }
    }

    pub(crate) fn fail_recv(&mut self, e: String) {
        self.stop_recv();
        self.recv.st = St::Error;
        self.recv.error = e.clone();
        self.set_status(e, "bad");
    }

    pub(crate) fn stop_recv(&mut self) {
        self.recv.cancel.store(true, Ordering::Relaxed);
        self.recv.jobs = None;
        self.recv.proc = None;
        self.recv.st = St::Off;
        self.recv.files.clear();
        self.recv.states.clear();
    }

    pub(crate) fn queue(&mut self, ids: &[u64]) {
        let Some(jobs) = &self.recv.jobs else { return };
        let mut n = 0;
        for id in ids {
            let busy = self.recv.states.get(id).is_some_and(|s| s.st == "queued" || s.st == "downloading");
            if busy {
                continue;
            }
            if let Some(file) = self.recv.files.iter().find(|f| f.id == *id).cloned() {
                let total = file.size;
                if jobs.send(share::Job { file, dest: self.recv.inbox.clone() }).is_ok() {
                    self.recv.states.insert(
                        *id,
                        FileState { st: "queued", done: 0, total, speed: Speed::new(0), path: None, error: String::new() },
                    );
                    n += 1;
                }
            }
        }
        if n > 0 {
            self.set_status(format!("Downloading {n} file{}…", plural(n)), "busy");
        }
    }
}
