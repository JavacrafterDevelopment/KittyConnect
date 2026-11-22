//! The Hub tab: what can be invited, joining invites, and the
//! Connections list.

use crate::*;

impl App {
    /// Everything open on this PC that has a code, which the hub can invite
    /// people to: the room we're in, our file share, and what we host in
    /// Port Forward and Mounts. The codes are made here; the page only says
    /// which one.
    pub(crate) fn invitables(&self) -> Vec<Invitable> {
        let mut out = Vec::new();
        if let Some((code, title)) = self.voip.invite_code() {
            out.push(Invitable { what: "voip".into(), kind: "voip", title, code });
        }
        if let (St::Live, Some(addr), Some(srv)) = (self.share.st, &self.share.addr, &self.share.server) {
            let n = self.share.lib.lock().unwrap().len();
            out.push(Invitable {
                what: "share".into(),
                kind: "file",
                title: format!("{n} file{}", plural(n)),
                code: code::file(srv.port, addr),
            });
        }
        for h in self.hosts.iter().filter(|h| h.st == St::Live && h.proc.is_some()) {
            let Some(addr) = &h.addr else { continue };
            let (kind, title, code) = match (&h.game, &h.mount) {
                (Some(g), _) => ("game", format!("{} · port {}", game_name(g), h.ports[0]), code::game(g, h.ports[0], addr)),
                (_, Some(m)) => (
                    "mount",
                    m.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Shared folder".into()),
                    code::mount(h.ports[0], addr),
                ),
                // An invite goes only to the people picked, inside the hub's
                // tunnel, so it carries the port numbers: they join with one press.
                _ => ("port", ports_title(&h.ports), format!("kitty:port:{}:{addr}", join_ports(&h.ports, ","))),
            };
            out.push(Invitable { what: format!("host:{}", h.key), kind, title, code });
        }
        out
    }

    /// Joins what an invite is to, the same way pasting its code in that tab
    /// would. Returns the tab (and Port Forward view) to show, where any error lands.
    pub(crate) fn join_invite(&mut self, a: &hub::Accepted) -> (&'static str, &'static str) {
        match a.kind {
            "voip" => {
                if self.voip.is_on() {
                    self.voip.leave();
                }
                self.voip.join(&a.code, &self.name, &self.proxy);
                self.set_status("Joining the room through TailCat…", "busy");
                ("voip", "")
            }
            "file" => {
                self.recv_bye();
                self.start_recv(&a.code);
                ("files", "")
            }
            "mount" => {
                self.start_port_join(&a.code, "", &[], false, JoinFrom::Mount);
                self.mount_join_error = std::mem::take(&mut self.join_error);
                ("mounts", "")
            }
            "game" => {
                self.start_port_join(&a.code, "", &[], false, JoinFrom::Game);
                ("ports", "games")
            }
            _ => {
                self.start_port_join(&a.code, "", &[], false, JoinFrom::Manual);
                ("ports", "manual")
            }
        }
    }

    /// The Drop button on a row of the Connections list: the same as that
    /// tab's own Stop, Disconnect or Leave (or, for one person in our room or
    /// hub, Remove).
    pub(crate) fn drop_connection(&mut self, id: &str) {
        let (what, n) = id.split_once(':').unwrap_or((id, ""));
        let n: u64 = n.parse().unwrap_or(0);
        match what {
            "hub" => {
                let closing = self.hub.is_host();
                self.hub.leave();
                self.set_status(if closing { "Closed your hub. Its code no longer works." } else { "Left the hub" }, "idle");
            }
            "hub-member" => self.hub.kick(n),
            "share" => {
                self.stop_share();
                self.set_status("Stopped sharing. The old code no longer works.", "idle");
            }
            "recv" => {
                self.recv_bye();
                self.stop_recv();
                self.set_status("Disconnected", "idle");
            }
            "host" => {
                self.hosts.retain(|h| h.key != n);
                self.set_status("Closed. Its code no longer works.", "idle");
            }
            "join" => {
                if let Some(j) = self.joins.iter_mut().find(|j| j.key == n) {
                    join_bye(j);
                }
                self.joins.retain(|j| j.key != n);
                self.set_status("Disconnected", "idle");
            }
            "voip" => {
                self.voip.leave();
                self.set_status("Left the room", "idle");
            }
            "voip-member" => self.voip.moderate("kick", n.min(u16::MAX as u64) as u16),
            _ => {}
        }
    }

    /// Every connection open anywhere in the app, for the Hub tab: what it
    /// is, who's on it, and how each is connected (direct or relay). Every
    /// one goes through a TailCat tunnel: the official TailCat, or for voice
    /// rooms the VOIP helper (TailCat's own library, for UDP).
    pub(crate) fn connections(&self) -> Value {
        let mut rows = Vec::new();
        let person = |m: &Value, drop: Option<String>| json!({ "name": m["name"], "path": m["path"], "rtt": m["rtt"], "drop": drop });
        // A saved hub shows who's connected in its own tile, not here too.
        if self.hub.is_on() && !self.hub.is_saved() {
            let host = self.hub.is_host();
            let people: Vec<Value> = self
                .hub
                .others()
                .into_iter()
                .map(|m| person(m, (host && m["host"] != true).then(|| format!("hub-member:{}", m["id"].as_u64().unwrap_or(0)))))
                .collect();
            rows.push(json!({
                "id": "hub", "section": "Hub", "title": self.hub.title(), "role": if host { "Hosting" } else { "Joined" },
                "link": (!host).then(|| self.hub.link.json()), "people": people, "via": "official",
                "dropLabel": match (host, self.hub.is_saved()) { (true, true) => "Close", (true, false) => "Delete", _ => "Leave" },
            }));
        }
        if self.share.st != St::Off {
            let people: Vec<Value> = self.share.peers.json().as_array().cloned().unwrap_or_default().iter().map(|p| person(p, None)).collect();
            rows.push(json!({
                "id": "share", "section": "File Share", "title": "Sharing files", "role": "Hosting",
                "link": Value::Null, "people": people, "via": "official", "dropLabel": "Stop",
            }));
        }
        if self.recv.st != St::Off {
            rows.push(json!({
                "id": "recv", "section": "File Share", "title": "Receiving files", "role": "Joined",
                "host": self.recv.host_name, "link": self.recv.link.json(), "people": [], "via": "official", "dropLabel": "Disconnect",
            }));
        }
        for h in &self.hosts {
            let (section, title) = match (&h.game, &h.mount) {
                (Some(g), _) => ("Game servers", format!("{} · port {}", game_name(g), h.ports[0])),
                (_, Some(m)) => ("Mounts", m.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
                _ => ("Port Forward", ports_title(&h.ports)),
            };
            let people: Vec<Value> = h.peers.json().as_array().cloned().unwrap_or_default().iter().map(|p| person(p, None)).collect();
            rows.push(json!({
                "id": format!("host:{}", h.key), "section": section, "title": title, "role": "Hosting",
                "link": Value::Null, "people": people, "via": "official", "dropLabel": "Stop",
            }));
        }
        for j in &self.joins {
            let remotes: Vec<u16> = j.maps.iter().map(|(_, r)| *r).collect();
            let (section, title) = match (&j.game, &j.mount) {
                (Some(g), _) => ("Game servers", game_name(g).to_string()),
                (_, Some(m)) => ("Mounts", m.info.as_ref().map(|i| i.title.clone()).unwrap_or_else(|| "Shared folder".into())),
                _ => ("Port Forward", ports_title(&remotes)),
            };
            rows.push(json!({
                "id": format!("join:{}", j.key), "section": section, "title": title, "role": "Joined",
                "host": j.host_name, "link": (j.st == St::Live).then(|| j.link.json()), "people": [], "via": "official",
                "dropLabel": "Disconnect",
            }));
        }
        if let Some(v) = self.voip.connection() {
            let host = self.voip.is_hosting();
            let me = v["me"].as_u64();
            let people: Vec<Value> = v["members"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter(|m| m["id"].as_u64() != me)
                .map(|m| person(m, (host && m["host"] != true).then(|| format!("voip-member:{}", m["id"].as_u64().unwrap_or(0)))))
                .collect();
            let room = v["room"].as_str().filter(|r| !r.is_empty()).unwrap_or("Voice room");
            rows.push(json!({
                "id": "voip", "section": "VOIP", "title": room, "role": if host { "Hosting" } else { "Joined" },
                "link": (!host).then(|| v["link"].clone()), "people": people, "via": "voip", "dropLabel": "Leave",
            }));
        }
        json!(rows)
    }
}
