// The Sandboxes display stream protocol against the golden file
// contracts/fixtures/sandboxes-stream.json (the TypeScript SDK reads the same
// file): every frame round-trips through the Rust types, tolerated frames
// parse to their canonical form, invalid frames are refused, chunking is
// byte-identical, and the mock machine reproduces every session, including
// that a stream token admits only its own sandbox.
#![cfg(feature = "stream")]

use std::collections::HashMap;

use serde_json::{json, Value};
use sylphx::sandboxes::stream::*;
use sylphx::sandboxes::stream_mock::*;

fn fixture() -> Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../contracts/fixtures/sandboxes-stream.json"
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// JSON numbers compare by value: `2` and `2.0` are one number.
fn norm(v: &Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => json!(f as i64),
            _ => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
        _ => v.clone(),
    }
}

fn round_trip(kind: &str, text: &str) -> Result<Value, ErrorCode> {
    fn v<T: serde::Serialize>(r: Result<T, ErrorCode>) -> Result<Value, ErrorCode> {
        r.map(|f| serde_json::to_value(f).unwrap())
    }
    match kind {
        "signal_client" => v(parse_signal_client(text)),
        "signal_server" => v(parse_signal_server(text)),
        "control_client" => v(parse_control_client(text)),
        "control_server" => v(parse_control_server(text)),
        "input" => v(parse_input(text)),
        "pointer" => v(parse_pointer(text)),
        "clipboard" => v(parse_clipboard(text)),
        other => panic!("unknown kind {other}"),
    }
}

#[test]
fn every_message_round_trips() {
    let fx = fixture();
    for (kind, frames) in fx["messages"].as_object().unwrap() {
        for frame in frames.as_array().unwrap() {
            let back = round_trip(kind, &frame.to_string())
                .unwrap_or_else(|e| panic!("{kind} {frame} refused: {e:?}"));
            assert_eq!(norm(&back), norm(frame), "{kind}");
        }
    }
}

#[test]
fn tolerated_frames_parse_to_canonical() {
    for case in fixture()["tolerated"].as_array().unwrap() {
        let kind = case["kind"].as_str().unwrap();
        let back = round_trip(kind, &case["in"].to_string()).unwrap();
        assert_eq!(norm(&back), norm(&case["canonical"]), "{case}");
    }
}

#[test]
fn invalid_frames_are_refused() {
    for case in fixture()["invalid"].as_array().unwrap() {
        let kind = case["kind"].as_str().unwrap();
        assert_eq!(
            round_trip(kind, &case["in"].to_string()),
            Err(ErrorCode::InvalidMessage),
            "{case}"
        );
    }
    assert_eq!(parse_input("{not json"), Err(ErrorCode::InvalidMessage));
}

#[test]
fn chunking_is_byte_identical_and_joins_back() {
    let fx = fixture();
    let c = &fx["chunking"];
    let clip = match parse_clipboard(&c["clip"].to_string()).unwrap() {
        Clipboard::Clip(clip) => clip,
        other => panic!("{other:?}"),
    };
    let chunks = clip.chunks(c["slice_bytes"].as_u64().unwrap() as usize);
    assert_eq!(serde_json::to_value(&chunks).unwrap(), c["chunks"]);
    let mut joiner = ChunkJoiner::default();
    let mut out = None;
    for ch in &chunks {
        let Clipboard::ClipChunk(ch) = ch else {
            panic!()
        };
        out = joiner.push(ch).unwrap();
    }
    assert_eq!(out, Some(clip));
}

#[test]
fn large_clips_travel_as_chunks_and_limits_hold() {
    let text = "粵語 Cantonese 🀄 ".repeat(8000); // ~170 KiB
    let clip = Clip {
        seq: 3,
        items: vec![ClipItem {
            mime: ClipMime::TextPlain,
            data: text.clone(),
        }],
    };
    assert_eq!(clip.check(), Ok(()));
    let frames = clip.frames();
    assert!(frames.len() > 10);
    let mut joiner = ChunkJoiner::default();
    let mut got = None;
    for f in &frames {
        let s = to_json(f);
        assert!(s.len() <= MAX_FRAME_BYTES, "a frame of {} bytes", s.len());
        if let Clipboard::ClipChunk(ch) = parse_clipboard(&s).unwrap() {
            got = joiner.push(&ch).unwrap();
        }
    }
    assert_eq!(got.unwrap().items[0].data, text);
    // A small clip is one frame.
    let small = Clip {
        seq: 1,
        items: vec![],
    };
    assert_eq!(small.frames(), vec![Clipboard::Clip(small.clone())]);

    let big_text = Clip {
        seq: 1,
        items: vec![ClipItem {
            mime: ClipMime::TextPlain,
            data: "a".repeat(MAX_CLIP_TEXT_BYTES + 1),
        }],
    };
    assert_eq!(big_text.check(), Err(ErrorCode::TooLarge));
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_CLIP_IMAGE_BYTES + 1]);
    let big_png = Clip {
        seq: 1,
        items: vec![ClipItem {
            mime: ClipMime::ImagePng,
            data: png,
        }],
    };
    assert_eq!(big_png.check(), Err(ErrorCode::TooLarge));
    let two_texts = Clip {
        seq: 1,
        items: vec![
            ClipItem {
                mime: ClipMime::TextPlain,
                data: "a".into(),
            },
            ClipItem {
                mime: ClipMime::TextPlain,
                data: "b".into(),
            },
        ],
    };
    assert_eq!(two_texts.check(), Err(ErrorCode::InvalidMessage));
}

#[test]
fn admission_refusals_carry_the_route_status() {
    assert_eq!(AdmitError::Unauthenticated.status(), 401);
    assert_eq!(AdmitError::NotFound.status(), 404);
    assert_eq!(AdmitError::StaleGeneration.status(), 409);
    assert_eq!(AdmitError::ControlHeld.status(), 409);
}

/// `{"$contains": [...]}` matches a string holding every part.
fn matches(expected: &Value, actual: &Value) -> bool {
    if let Some(parts) = expected.get("$contains").and_then(Value::as_array) {
        let Some(s) = actual.as_str() else {
            return false;
        };
        return parts.iter().all(|p| s.contains(p.as_str().unwrap()));
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            e.len() == a.len()
                && e.iter()
                    .all(|(k, v)| a.get(k).is_some_and(|av| matches(v, av)))
        }
        (Value::Array(e), Value::Array(a)) => {
            e.len() == a.len() && e.iter().zip(a).all(|(x, y)| matches(x, y))
        }
        _ => norm(expected) == norm(actual),
    }
}

struct Run {
    hub: MockStreamHub,
    tokens: HashMap<String, String>,
    peers: HashMap<String, SessionId>,
    chunks: Vec<Value>,
}

impl Run {
    fn new(fx: &Value) -> Run {
        let mut hub = MockStreamHub::new();
        for l in fx["hub"]["leases"].as_array().unwrap() {
            hub.add_lease(MockLeaseSpec {
                uid: l["uid"].as_str().unwrap().into(),
                org: l["org"].as_str().unwrap().into(),
                project: l["project"].as_str().unwrap().into(),
                generation: l["generation"].as_i64().unwrap(),
                holder: serde_json::from_value(l["holder"].clone()).unwrap(),
                epoch: l["epoch"].as_i64().unwrap(),
                label: l["label"].as_str().unwrap().into(),
                monitors: serde_json::from_value(l["monitors"].clone()).unwrap(),
            });
        }
        let mut tokens = HashMap::new();
        for (name, claims) in fx["hub"]["tokens"].as_object().unwrap() {
            let claims: MockClaims = serde_json::from_value(claims.clone()).unwrap();
            tokens.insert(name.clone(), hub.mint_token(claims));
        }
        Run {
            hub,
            tokens,
            peers: HashMap::new(),
            chunks: fx["chunking"]["chunks"].as_array().unwrap().clone(),
        }
    }

    fn token(&self, name: &str) -> String {
        let name = name.strip_prefix('$').unwrap_or(name);
        self.tokens
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    fn peer_name(&self, id: SessionId) -> String {
        self.peers
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.clone())
            .unwrap()
    }

    fn outputs(&mut self) -> Value {
        let out = self.hub.drain();
        Value::Array(
            out.into_iter()
                .map(|o| {
                    json!({
                        "peer": self.peer_name(o.session),
                        "ch": serde_json::to_value(o.channel).unwrap(),
                        "msg": o.frame,
                    })
                })
                .collect(),
        )
    }

    fn state(&self, lease: &str) -> Value {
        let l = self.hub.lease(lease).unwrap();
        json!({
            "probe": if l.probe_white { "white" } else { "black" },
            "held_keys": l.held_keys,
            "held_buttons": l.held_buttons,
            "typed": l.typed,
            "pointer": l.pointer.as_ref().map(|(m, x, y)| json!({"m": m, "x": x, "y": y})),
            "clipboard": serde_json::to_value(&l.clipboard).unwrap(),
            "combos": l.combos,
            "controller": l.controller.map(|c| self.peer_name(c)),
        })
    }

    fn step(&mut self, session: &str, i: usize, step: &Value) {
        let at = format!("{session} step {i}: {step}");
        if let Some(peer) = step.get("admit").and_then(Value::as_str) {
            let token = self.token(step["token"].as_str().unwrap());
            let got = match self.hub.admit(&token) {
                Ok(id) => {
                    self.peers.insert(peer.to_string(), id);
                    json!("ok")
                }
                Err(e) => serde_json::to_value(e).unwrap(),
            };
            assert_eq!(got, step["expect"], "{at}");
            assert_eq!(self.outputs(), json!([]), "{at}");
            return;
        }
        if let Some(lease) = step.get("state").and_then(Value::as_str) {
            let state = self.state(lease);
            for (k, v) in step["expect"].as_object().unwrap() {
                assert!(matches(v, &state[k]), "{at}: {k} is {}", state[k]);
            }
            return;
        }
        if let Some(peer) = step.get("peer").and_then(Value::as_str) {
            let id = self.peers[peer];
            let ch: Channel = serde_json::from_value(step["ch"].clone()).unwrap();
            let text = match step.get("send_raw") {
                Some(raw) => raw.as_str().unwrap().to_string(),
                None => {
                    let mut msg = match step.get("chunk") {
                        Some(chunk) => self.chunks[chunk.as_u64().unwrap() as usize].clone(),
                        None => step["send"].clone(),
                    };
                    if msg["t"] == "upgrade" {
                        msg["token"] = json!(self.token(msg["token"].as_str().unwrap()));
                    }
                    msg.to_string()
                }
            };
            self.hub.send(id, ch, &text);
        } else {
            let lease = step["lease"].as_str().unwrap_or("");
            match step["hub"].as_str().unwrap() {
                "acquire" => {
                    let holder: Holder = serde_json::from_value(step["holder"].clone()).unwrap();
                    self.hub
                        .acquire(lease, holder, step["label"].as_str().unwrap());
                }
                "release" => self.hub.release(lease),
                "bump_generation" => self.hub.bump_generation(lease),
                "end_lease" => self.hub.end_lease(lease),
                "revoke" => {
                    let token = self.token(step["token"].as_str().unwrap());
                    self.hub.revoke(&token);
                }
                "machine_clipboard" => {
                    let items: Vec<ClipItem> =
                        serde_json::from_value(step["items"].clone()).unwrap();
                    self.hub.set_machine_clipboard(lease, items);
                }
                other => panic!("{at}: unknown hub step {other}"),
            }
        }
        let got = self.outputs();
        assert!(
            matches(&step["expect"], &got),
            "{at}\n  got {got}\n  want {}",
            step["expect"]
        );
    }
}

#[test]
fn the_mock_peer_reproduces_every_session() {
    let fx = fixture();
    let sessions = fx["sessions"].as_array().unwrap();
    for s in sessions {
        let mut run = Run::new(&fx);
        let name = s["name"].as_str().unwrap();
        for (i, step) in s["steps"].as_array().unwrap().iter().enumerate() {
            run.step(name, i, step);
        }
    }
}

/// Tenant isolation, stated: a stream token only ever admits its own
/// sandbox. The token names one lease in one tenant; a session reaches that
/// lease only, and another lease's controller token cannot drive it.
#[test]
fn a_stream_token_admits_only_its_own_sandbox() {
    let mut hub = MockStreamHub::new();
    hub.add_lease(MockLeaseSpec::desktop("lease_a", "org_1", "proj_1", 1));
    hub.add_lease(MockLeaseSpec::desktop("lease_b", "org_2", "proj_2", 1));
    let token_b = hub.mint_token(MockClaims::viewer("lease_b", "org_2", "proj_2", 1));
    // lease_a's uid under lease_b's tenant: the tenant-scoped read finds nothing.
    let forged = hub.mint_token(MockClaims::viewer("lease_a", "org_2", "proj_2", 1));
    assert_eq!(hub.admit(&forged), Err(AdmitError::NotFound));

    let peer = hub.admit(&token_b).unwrap();
    let hello =
        r#"{"t":"hello","v":1,"client":{"dpr":1,"w":800,"h":600,"touch":false},"monitors":"all"}"#;
    hub.send(peer, Channel::Signal, hello);
    hub.send(peer, Channel::Signal, r#"{"t":"answer","sdp":"v=0\r\n"}"#);
    hub.drain();
    // A human holds lease_a at epoch 1, and lease_b at epoch 1.
    hub.acquire("lease_a", Holder::Human, "Ana");
    hub.acquire("lease_b", Holder::Human, "Bob");
    hub.drain();
    let ctl_a = hub.mint_token(MockClaims::controller("lease_a", "org_1", "proj_1", 1, 1));
    hub.send(
        peer,
        Channel::Control,
        &json!({"t": "upgrade", "token": ctl_a}).to_string(),
    );
    let out = hub.drain();
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0].frame,
        json!({"t": "error", "code": "unauthenticated"})
    );
    hub.send(peer, Channel::Input, r#"{"t":"tx","text":"x"}"#);
    assert_eq!(hub.lease("lease_a").unwrap().typed, "");
    assert_eq!(hub.lease("lease_b").unwrap().typed, "");
    assert_eq!(hub.lease("lease_a").unwrap().controller, None);
}
