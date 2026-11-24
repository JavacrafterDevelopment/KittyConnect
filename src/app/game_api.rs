//! The game API's requests, answered from what the app knows: the hub,
//! voice, and game sessions (see [`crate::api`]).

use crate::api::{err, ok, ApiEvent, Target};
use crate::*;

impl App {
    pub(crate) fn on_api(&mut self, ev: ApiEvent) -> bool {
        match ev {
            ApiEvent::Open { conn, tx, exe, hello } => {
                self.api.open(conn, tx, exe, &hello);
                self.api_push();
            }
            ApiEvent::Closed { conn } => self.api.closed(conn),
            ApiEvent::Msg { conn, msg } => self.api_request(conn, &msg),
        }
        true
    }

    fn api_request(&mut self, conn: u64, req: &Value) {
        let Some((app, _)) = self.api.who(conn, req) else { return };
        let id = &req["id"];
        let cmd = req["cmd"].as_str().unwrap_or_default();
        let reply = match cmd {
            "getMe" => ok(id, json!({ "name": self.name, "inHub": self.hub.is_live(), "inVoice": self.voip.is_live() })),
            "getHub" => ok(id, self.api_hub()),
            "getSavedHubs" => ok(id, self.api_saved()),
            "getVoice" => ok(id, self.voip.api_json()),
            "getSessions" => ok(id, api_sessions(&self.hub, &app)),
            "hostSession" => {
                if !self.hub.is_live() {
                    err(id, "Join or start a hub in Portside first: sessions are shown to the hub you're in.")
                } else if !self.hub.has_games() {
                    err(id, "This hub's host runs an older Portside, which can't show game sessions. Update it.")
                } else {
                    return self.api.host(conn, req, &app, &self.proxy);
                }
            }
            "updateSession" => return self.api.update_hosting(conn, req),
            "stopSession" => return self.api.stop_hosting(conn, req),
            "joinSession" => match self.api_target(&app, req["session"].as_str().unwrap_or_default()) {
                Some(t) => return self.api.join(conn, req, t, &self.proxy),
                None => err(id, "No such session in the hub you're in (for this game)."),
            },
            "leaveSession" => return self.api.leave(conn, req),
            "sendMessage" => match req.get("data") {
                None => err(id, "sendMessage needs data."),
                Some(d) if d.to_string().len() > 16 * 1024 => err(id, "data must be at most 16 KB of JSON."),
                Some(d) => {
                    let to = req["to"].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
                    match self.hub.send_app(&app, to, d.clone()) {
                        Ok(()) => ok(id, json!({})),
                        Err(e) => err(id, e),
                    }
                }
            },
            "setActivity" => {
                let text = hub::clean(req["text"].as_str().unwrap_or_default(), 60);
                self.api.set_activity(conn, Some(text));
                ok(id, json!({}))
            }
            "clearActivity" => {
                self.api.set_activity(conn, None);
                ok(id, json!({}))
            }
            _ => err(id, format!("Unknown cmd \"{cmd}\".")),
        };
        self.api.reply(conn, reply);
    }

    /// The hub we're in, as games see it (null when we're in none).
    fn api_hub(&self) -> Value {
        if !self.hub.is_live() {
            return Value::Null;
        }
        let me = self.hub.my_id();
        json!({
            "id": self.hub.hub_id(),
            "name": self.hub.title(),
            "saved": self.hub.is_saved(),
            "host": self.hub.is_host(),
            "me": me,
            "games": self.hub.has_games(),
            "members": self.hub.member_list().iter().map(|m| json!({
                "id": m["id"], "name": m["name"], "host": m["host"], "me": m["id"].as_u64() == Some(me),
                "path": m["path"], "rtt": m["rtt"], "activity": m["activity"],
            })).collect::<Vec<_>>(),
        })
    }

    fn api_saved(&self) -> Value {
        let here = self.hub.is_live().then(|| self.hub.hub_id().to_string());
        json!(self
            .hub
            .saved_list()
            .into_iter()
            .map(|(id, name, left)| json!({ "id": id, "name": name, "expiresIn": left, "open": here.as_deref() == Some(id.as_str()) }))
            .collect::<Vec<_>>())
    }

    /// A session in the hub, for this game, by "<member id>:<sid>".
    fn api_target(&self, app: &str, session: &str) -> Option<Target> {
        if !self.hub.is_live() {
            return None;
        }
        let g = self.hub.game_sessions().iter().find(|g| g["app"] == app && session_id(g) == session)?;
        Some(Target {
            id: session.to_string(),
            app: app.to_string(),
            title: g["title"].as_str().unwrap_or_default().to_string(),
            host: g["name"].as_str().unwrap_or_default().to_string(),
            port: g["port"].as_u64()? as u16,
            addr: g["addr"].as_str()?.to_string(),
        })
    }

    /// Whatever changed, each game hears (only what's new to it).
    pub(crate) fn api_push(&mut self) {
        if !self.api.has_ready() {
            return;
        }
        let hub_view = self.api_hub();
        let voice = self.voip.api_json();
        self.api.push("hub", |_| hub_view.clone());
        self.api.push("voice", |_| voice.clone());
        let hub = &self.hub;
        self.api.push("sessions", |app| api_sessions(hub, app));
    }

    /// Once a second: the hub hears our games' sessions and activity.
    pub(crate) fn api_tick(&mut self) -> bool {
        let changed = self.api.tick();
        self.hub.sync_sessions(self.api.announce());
        self.hub.set_activity(self.api.activity());
        self.api_push();
        changed
    }
}

fn session_id(g: &Value) -> String {
    format!("{}:{}", g["from"].as_u64().unwrap_or(0), g["sid"].as_str().unwrap_or_default())
}

/// The sessions in the hub for one game, as it sees them: no tunnel
/// addresses (Portside joins for it).
fn api_sessions(hub: &hub::Hub, app: &str) -> Value {
    if !hub.is_live() {
        return json!([]);
    }
    let me = hub.my_id();
    json!(hub
        .game_sessions()
        .iter()
        .filter(|g| g["app"] == app)
        .map(|g| json!({
            "id": session_id(g), "sid": g["sid"], "title": g["title"], "info": g["info"],
            "host": { "id": g["from"], "name": g["name"] },
            "mine": g["from"].as_u64() == Some(me),
        }))
        .collect::<Vec<_>>())
}
