//! Shared folders: hosting one, and mounting, browsing and copying
//! another's.

use crate::*;

impl App {
    /// Shares the chosen folder: its WebDAV server, served through TailCat
    /// beside the beacon. Read only unless `write`.
    pub(crate) fn start_mount_host(&mut self, write: bool) {
        self.mount_host_error.clear();
        let Some(dir) = self.mount_pick.clone() else {
            return self.mount_host_error = "Choose a folder first.".into();
        };
        if self.hosts.iter().any(|h| h.proc.is_some() && h.mount.as_ref().is_some_and(|m| m.dir == dir)) {
            return self.mount_host_error = "That folder is already shared below.".into();
        }
        let server = match mount::MountServer::start(&dir, write) {
            Ok(s) => s,
            Err(e) => return self.mount_host_error = e,
        };
        let mut list = vec![server.port.to_string()];
        if let Some(b) = beacon::ensure(&self.proxy) {
            list.push(b.to_string());
        }
        let args = vec!["serve".into(), "--key=new".into(), list.join(",")];
        match tailcat::spawn(Role::PortHost, &args, &self.proxy) {
            Ok(p) => {
                let key = self.key();
                self.hosts.insert(0, PortHost {
                    key,
                    st: St::Starting,
                    error: String::new(),
                    warn: String::new(),
                    proc: Some(p),
                    ports: vec![server.port],
                    addr: None,
                    link: Link::default(),
                    peers: Peers::default(),
                    game: None,
                    mount: Some(MountShare { dir, write, server }),
                    // Nothing to check: the server is ours, and it's up.
                    probed: true,
                    lan_note: String::new(),
                    log: Log::default(),
                });
                self.mount_pick = None;
                self.set_status("Starting TailCat…", "busy");
            }
            Err(e) => self.mount_host_error = e,
        }
    }

    /// Once a joined mount's host has answered, asks it for the folder's name
    /// and whether changes are allowed (again every few seconds until it says).
    pub(crate) fn mounts_tick(&mut self, now: Instant) {
        for j in self.joins.iter_mut().filter(|j| j.st == St::Live && j.proc.is_some()) {
            let Some(m) = j.mount.as_mut() else { continue };
            if m.info.is_some() || m.asking || m.asked_at.is_some_and(|t| now.duration_since(t) < Duration::from_secs(3)) {
                continue;
            }
            m.asking = true;
            m.asked_at = Some(now);
            let (key, port, proxy) = (j.key, j.maps[0].0, self.proxy.clone());
            std::thread::spawn(move || {
                let _ = proxy.send_event(UserEvent::MountInfo(key, mount::info(port)));
            });
        }
    }

    pub(crate) fn mount_of(&mut self, key: u64) -> Option<(u16, &mut MountJoin)> {
        let j = self.joins.iter_mut().find(|j| j.key == key && j.proc.is_some())?;
        let port = j.maps[0].0;
        Some((port, j.mount.as_mut()?))
    }

    pub(crate) fn on_mount_info(&mut self, key: u64, r: Result<mount::Info, String>) -> bool {
        let Some((_, m)) = self.mount_of(key) else { return false };
        m.asking = false;
        match r {
            Ok(info) => {
                m.info = Some(info);
                m.error.clear();
            }
            Err(e) => m.error = e,
        }
        true
    }

    /// Mounts it as a drive (Windows) or a volume (macOS), off the UI thread.
    pub(crate) fn mount_map(&mut self, key: u64) {
        let proxy = self.proxy.clone();
        let Some((port, m)) = self.mount_of(key) else { return };
        let Some(name) = m.info.as_ref().map(|i| i.name.clone()) else { return };
        if m.mounted.is_some() || !m.busy.is_empty() {
            return;
        }
        m.busy = "mounting";
        m.error.clear();
        std::thread::spawn(move || {
            let _ = proxy.send_event(UserEvent::MountDone(key, mount::mount(port, &name).map(Some)));
        });
    }

    pub(crate) fn mount_unmap(&mut self, key: u64) {
        let proxy = self.proxy.clone();
        let Some((_, m)) = self.mount_of(key) else { return };
        let Some(mounted) = m.mounted.clone() else { return };
        m.busy = "unmounting";
        std::thread::spawn(move || {
            mount::unmount(&mounted);
            let _ = proxy.send_event(UserEvent::MountDone(key, Ok(None)));
        });
    }

    pub(crate) fn on_mount_done(&mut self, key: u64, r: Result<Option<mount::Mounted>, String>) -> bool {
        let Some((_, m)) = self.mount_of(key) else {
            // Gone meanwhile (disconnected): don't leave it mounted.
            if let Ok(Some(mounted)) = r {
                std::thread::spawn(move || mount::unmount(&mounted));
            }
            return false;
        };
        m.busy = "";
        let status = match r {
            Ok(Some(mounted)) => {
                let label = mounted.label();
                m.mounted = Some(mounted.clone());
                mount::open(&mounted);
                (format!("Mounted as {label}."), "ok")
            }
            Ok(None) => {
                m.mounted = None;
                ("Unmounted.".to_string(), "idle")
            }
            Err(e) => {
                m.error = e.clone();
                (e, "bad")
            }
        };
        self.set_status(status.0, status.1);
        true
    }

    /// Opens (or moves) the built-in browser to `path` and lists it.
    pub(crate) fn mount_browse(&mut self, key: u64, path: Option<String>) {
        let proxy = self.proxy.clone();
        let Some((port, m)) = self.mount_of(key) else { return };
        let path = path.or_else(|| m.browse.as_ref().map(|b| b.path.clone())).unwrap_or_default();
        let b = m.browse.get_or_insert_with(|| Browse { path: String::new(), entries: Vec::new(), loading: false, error: String::new() });
        b.path = path.clone();
        b.loading = true;
        b.error.clear();
        std::thread::spawn(move || {
            let r = mount::list(port, &path);
            let _ = proxy.send_event(UserEvent::MountList(key, path, r));
        });
    }

    pub(crate) fn on_mount_list(&mut self, key: u64, path: String, r: Result<Vec<mount::Entry>, String>) -> bool {
        let Some((_, m)) = self.mount_of(key) else { return false };
        let Some(b) = m.browse.as_mut().filter(|b| b.path == path) else { return false };
        b.loading = false;
        match r {
            Ok(entries) => b.entries = entries,
            Err(e) => b.error = e,
        }
        true
    }

    pub(crate) fn new_transfer(&mut self, name: String, up: bool) -> (u64, Arc<AtomicBool>, Transfer) {
        let id = self.next_xfer;
        self.next_xfer += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let t = Transfer { id, name, up, done: 0, total: 0, st: "going", error: String::new(), path: None, speed: Speed::new(0), cancel: cancel.clone() };
        (id, cancel, t)
    }

    /// Downloads a file, or a folder with everything in it, into the Downloads
    /// folder (or wherever File Share saves).
    pub(crate) fn mount_get(&mut self, key: u64, path: &str, dir: bool) {
        let inbox = self.recv.inbox.clone();
        let label = path.rsplit('/').next().unwrap_or(path).to_string();
        let (id, cancel, t) = self.new_transfer(if dir { format!("{label}/") } else { label.clone() }, false);
        let proxy = self.proxy.clone();
        let Some((port, m)) = self.mount_of(key) else { return };
        let Some(name) = m.info.as_ref().map(|i| i.name.clone()) else { return };
        m.transfers.insert(0, t);
        m.transfers.truncate(30);
        let path = path.to_string();
        std::thread::spawn(move || {
            let send = |x: Xfer| {
                let _ = proxy.send_event(UserEvent::MountXfer(key, id, x));
            };
            let result = if dir {
                // Every file under it, keeping the layout, into <inbox>/<folder>.
                let dest = crate::share::safe_relative(&label).map(|r| crate::share::unique(&inbox.join(r)));
                match dest {
                    None => Err("The folder name isn't safe to save.".to_string()),
                    Some(dest) => {
                        let mut files = Vec::new();
                        let mut todo = vec![(path.clone(), dest.clone())];
                        let mut listed = Ok(());
                        while let Some((rel, to)) = todo.pop() {
                            match mount::list(port, &rel) {
                                Ok(entries) => {
                                    for e in entries {
                                        let (r, Some(sub)) = (format!("{rel}/{}", e.name), crate::share::safe_relative(&e.name)) else { continue };
                                        if e.dir {
                                            todo.push((r, to.join(sub)));
                                        } else {
                                            files.push((r, to.clone(), e.size));
                                        }
                                    }
                                }
                                Err(e) => {
                                    listed = Err(e);
                                    break;
                                }
                            }
                        }
                        let total: u64 = files.iter().map(|f| f.2).sum();
                        let mut done_before = 0u64;
                        let mut result = listed;
                        if result.is_ok() {
                            let _ = std::fs::create_dir_all(&dest);
                            for (rel, to, size) in files {
                                let mut last = std::time::Instant::now();
                                let r = mount::download(port, &name, &rel, &to, &cancel, &mut |d, _| {
                                    if last.elapsed() >= Duration::from_millis(200) {
                                        last = std::time::Instant::now();
                                        send(Xfer::Progress(done_before + d, total));
                                    }
                                });
                                if let Err(e) = r {
                                    result = Err(e);
                                    break;
                                }
                                done_before += size;
                                send(Xfer::Progress(done_before, total));
                            }
                        }
                        result.map(|_| Some(dest))
                    }
                }
            } else {
                let mut last = std::time::Instant::now();
                mount::download(port, &name, &path, &inbox, &cancel, &mut |d, t| {
                    if last.elapsed() >= Duration::from_millis(200) {
                        last = std::time::Instant::now();
                        send(Xfer::Progress(d, t));
                    }
                })
                .map(Some)
            };
            send(Xfer::Done(result));
        });
    }

    /// Uploads files into the browser's folder (when the host allows changes).
    pub(crate) fn mount_put(&mut self, key: u64, path: &str, files: Vec<PathBuf>) {
        let mut queued = Vec::new();
        for f in files.into_iter().filter(|f| f.is_file()) {
            let label = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let (id, cancel, t) = self.new_transfer(label, true);
            queued.push((id, cancel, f, t));
        }
        let proxy = self.proxy.clone();
        let Some((port, m)) = self.mount_of(key) else { return };
        let Some(name) = m.info.as_ref().filter(|i| i.write).map(|i| i.name.clone()) else { return };
        let mut jobs = Vec::new();
        for (id, cancel, f, t) in queued {
            m.transfers.insert(0, t);
            jobs.push((id, cancel, f));
        }
        m.transfers.truncate(30);
        let path = path.to_string();
        std::thread::spawn(move || {
            for (id, cancel, f) in jobs {
                let mut last = std::time::Instant::now();
                let r = mount::upload(port, &name, &path, &f, &cancel, &mut |d, t| {
                    if last.elapsed() >= Duration::from_millis(200) {
                        last = std::time::Instant::now();
                        let _ = proxy.send_event(UserEvent::MountXfer(key, id, Xfer::Progress(d, t)));
                    }
                });
                let _ = proxy.send_event(UserEvent::MountXfer(key, id, Xfer::Done(r.map(|_| None))));
            }
        });
    }

    pub(crate) fn on_mount_xfer(&mut self, key: u64, id: u64, x: Xfer) -> bool {
        let mut refresh = false;
        let Some((_, m)) = self.mount_of(key) else { return false };
        let Some(t) = m.transfers.iter_mut().find(|t| t.id == id) else { return false };
        match x {
            Xfer::Progress(done, total) => {
                t.done = done;
                t.total = total;
                t.speed.update(done);
            }
            Xfer::Done(Ok(path)) => {
                t.st = "done";
                t.done = t.total.max(t.done);
                t.path = path;
                refresh = t.up;
            }
            Xfer::Done(Err(e)) => {
                t.st = "error";
                t.error = e;
            }
        }
        // An upload finished: show it in the browser.
        if refresh && m.browse.is_some() {
            self.mount_browse(key, None);
        }
        true
    }
}
