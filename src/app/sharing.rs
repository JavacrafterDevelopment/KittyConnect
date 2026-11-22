//! Sharing files: hosting a share.

use crate::*;

impl App {
    pub(crate) fn start_share(&mut self) {
        if self.share.st == St::Starting || self.share.st == St::Live {
            return;
        }
        if self.share.lib.lock().unwrap().is_empty() {
            self.share.st = St::Error;
            self.share.error = "Add some files or a folder first.".into();
            return;
        }
        self.share.error.clear();
        self.share.log = Log::default();
        self.share.link = Link::default();
        self.share.peers.clear();
        let server = match Server::start(self.share.lib.clone(), self.proxy.clone()) {
            Ok(s) => s,
            Err(e) => return self.fail_share(e),
        };
        let args = vec!["serve".into(), "--key=new".into(), server.port.to_string()];
        self.share.log.push(format!("Portside: file server on 127.0.0.1:{}, starting TailCat", server.port));
        match tailcat::spawn(Role::ShareHost, &args, &self.proxy) {
            Ok(p) => {
                self.share.proc = Some(p);
                self.share.server = Some(server);
                self.share.st = St::Starting;
                self.set_status("Starting TailCat…", "busy");
            }
            Err(e) => self.fail_share(e),
        }
    }

    pub(crate) fn fail_share(&mut self, e: String) {
        self.share.st = St::Error;
        self.share.error = e.clone();
        self.set_status(e, "bad");
    }

    pub(crate) fn stop_share(&mut self) {
        self.share.st = St::Off;
        self.share.proc = None;
        self.share.server = None;
        self.share.addr = None;
        self.share.sending.clear();
        self.share.peers.clear();
    }
}
