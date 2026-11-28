//! Scenario tests of the index: synthetic seats and transcripts in a temp dir, driven through the
//! index's public API (`open`, `refresh`, `price_all`, `dated_prices`, the queries). Money is
//! computed by hand in the comments. `World` is the fixture, shared with the limits tests.

#[cfg(test)]
pub(crate) use tests::{World, account, assistant, at, dump, owner, weekly};

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::PathBuf;

    use chrono::{DateTime, TimeZone, Utc};

    use crate::claude::config_json::{Account, Utilization};
    use crate::claude::project::{Alias, Resolver};
    use crate::claude::seats::Seat;
    use crate::core::limits::{Reading, WindowKind};
    use crate::core::period::Span;
    use crate::core::pricing::{PriceBook, PriceEntry, PriceSource, RateOverrides, Rates};
    use crate::index::query::{Counted, Dim, Filter};
    use crate::index::{Index, SeatInfo};

    /// Seats and transcripts under a temp dir. Never a real config dir.
    pub(crate) struct World {
        pub dir: tempfile::TempDir,
        pub seats: Vec<Seat>,
    }

    pub(crate) fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// One assistant record (one content block of a response): `input` fresh input tokens,
    /// `output` output tokens, and the given tool uses.
    pub(crate) fn assistant(
        msg_id: &str,
        ts: &str,
        model: &str,
        cwd: &str,
        (input, output): (u64, u64),
        tools: &[&str],
    ) -> String {
        let content: Vec<serde_json::Value> = tools
            .iter()
            .map(|id| serde_json::json!({"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "ls"}}))
            .collect();
        serde_json::json!({
            "type": "assistant", "timestamp": ts, "cwd": cwd, "sessionId": "s",
            "message": {"id": msg_id, "model": model, "content": content,
                "usage": {"input_tokens": input, "output_tokens": output,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        })
        .to_string()
    }

    /// One assistant record of a response that calls these tools: (id, name, input).
    pub(crate) fn tool_calls(
        msg_id: &str,
        ts: &str,
        cwd: &str,
        tools: &[(&str, &str, serde_json::Value)],
    ) -> String {
        let content: Vec<serde_json::Value> = tools
            .iter()
            .map(|(id, name, input)| serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input}))
            .collect();
        serde_json::json!({
            "type": "assistant", "timestamp": ts, "cwd": cwd, "sessionId": "s",
            "message": {"id": msg_id, "model": "claude-sonnet-5-5", "content": content,
                "usage": {"input_tokens": 10, "output_tokens": 1,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        })
        .to_string()
    }

    fn prompt(ts: &str, text: &str) -> String {
        serde_json::json!({"type": "user", "timestamp": ts, "message": {"role": "user", "content": text}})
            .to_string()
    }

    fn api_error(ts: &str) -> String {
        serde_json::json!({"type": "assistant", "isApiErrorMessage": true, "error": "rate_limit", "timestamp": ts,
            "message": {"id": "x", "model": "<synthetic>", "content": [{"type": "text", "text": "API Error: Rate limit reached"}]}})
        .to_string()
    }

    pub(crate) fn owner(account: &str) -> String {
        serde_json::json!({"type": "session-owner", "accountUuid": account, "sessionId": "s"})
            .to_string()
    }

    pub(crate) fn compact(ts: &str) -> String {
        serde_json::json!({"type": "system", "subtype": "compact_boundary", "timestamp": ts})
            .to_string()
    }

    pub(crate) fn account(uuid: &str, since: &str) -> Account {
        Account {
            account_uuid: uuid.into(),
            profile_fetched_at: Some(at(since)),
            ..Default::default()
        }
    }

    pub(crate) fn weekly(account: &str, fetched: &str, used: f64, resets: &str) -> Utilization {
        Utilization {
            account_id: crate::core::ids::AccountId::new(account),
            org_from_profile: false,
            fetched_at: at(fetched),
            readings: vec![Reading {
                kind: WindowKind::SevenDay,
                fetched_at: at(fetched),
                utilization: used,
                resets_at: at(resets),
            }],
            extra_usage_enabled: false,
        }
    }

    impl World {
        pub(crate) fn new(labels: &[&str]) -> World {
            let dir = tempfile::tempdir().unwrap();
            let seats = labels
                .iter()
                .map(|l| {
                    let d = dir.path().join(format!("claude-{l}"));
                    std::fs::create_dir_all(d.join("projects").join("-w-proj")).unwrap();
                    Seat::new(l.to_string(), d.clone(), d.join(".claude.json"), false)
                })
                .collect();
            World { dir, seats }
        }

        /// The id of the seat labelled `label`.
        pub(crate) fn id(&self, label: &str) -> crate::core::ids::SeatId {
            self.seats
                .iter()
                .find(|s| s.label == label)
                .unwrap()
                .id
                .clone()
        }

        fn transcript(&self, seat: &str, session: &str) -> PathBuf {
            let s = self.seats.iter().find(|s| s.label == seat).unwrap();
            s.projects_dir()
                .join("-w-proj")
                .join(format!("{session}.jsonl"))
        }

        pub(crate) fn append(&self, seat: &str, session: &str, lines: &[String]) {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.transcript(seat, session))
                .unwrap();
            for l in lines {
                writeln!(f, "{l}").unwrap();
            }
        }

        /// The transcript of a subagent of `session`.
        fn subagent(&self, seat: &str, session: &str, agent: &str) -> PathBuf {
            let s = self.seats.iter().find(|s| s.label == seat).unwrap();
            let dir = s
                .projects_dir()
                .join("-w-proj")
                .join(session)
                .join("subagents");
            std::fs::create_dir_all(&dir).unwrap();
            dir.join(format!("agent-{agent}.jsonl"))
        }

        pub(crate) fn write_subagent(
            &self,
            seat: &str,
            session: &str,
            agent: &str,
            lines: &[String],
        ) {
            let p = self.subagent(seat, session, agent);
            let tmp = p.with_extension("tmp");
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, &p).unwrap();
        }

        /// Every transcript file of every seat.
        fn files(&self) -> Vec<crate::claude::files::TranscriptFile> {
            self.seats
                .iter()
                .flat_map(crate::claude::files::list)
                .collect()
        }

        /// Writes the transcript anew (another inode), as when a file is replaced.
        pub(crate) fn replace(&self, seat: &str, session: &str, lines: &[String]) {
            let p = self.transcript(seat, session);
            let tmp = p.with_extension("tmp");
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, &p).unwrap();
        }

        pub(crate) fn index(&self, name: &str) -> Index {
            Index::open(&self.dir.path().join(name)).unwrap()
        }

        pub(crate) fn refresh(&self, idx: &mut Index, info: &[SeatInfo], prices: &PriceBook) {
            let mut resolver = Resolver::new(vec![
                Alias {
                    prefix: "/w/shop".into(),
                    name: "shop".into(),
                },
                Alias {
                    prefix: "/w/docs".into(),
                    name: "docs".into(),
                },
            ]);
            // Tests name a seat's info by label; the index keys it by the seat's id.
            let info: Vec<SeatInfo> = info
                .iter()
                .cloned()
                .map(|mut i| {
                    if i.seat.as_str().is_empty() {
                        i.seat = self.id(&i.label);
                    }
                    i
                })
                .collect();
            idx.refresh(&self.seats, &info, &mut resolver, prices, &mut |_| {})
                .unwrap();
        }
    }

    /// Every row of a query, as text, for comparing two indexes.
    pub(crate) fn dump(idx: &Index, sql: &str) -> Vec<String> {
        let mut st = idx.conn.prepare(sql).unwrap();
        let n = st.column_count();
        st.query_map([], |r| {
            use rusqlite::types::ValueRef as V;
            Ok((0..n)
                .map(|i| match r.get_ref(i).unwrap() {
                    V::Null => "NULL".to_string(),
                    V::Integer(x) => x.to_string(),
                    V::Real(x) => format!("{x:.9}"),
                    V::Text(t) | V::Blob(t) => String::from_utf8_lossy(t).into_owned(),
                })
                .collect::<Vec<_>>()
                .join("|"))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    const REQUESTS: &str = "SELECT msg_id, seat, session_id, agent_id, account_uuid, ts, model, input, output, tools, cost, priced FROM requests ORDER BY msg_id";
    const SESSIONS: &str =
        "SELECT seat, session_id, first_ts, last_ts, compactions FROM sessions ORDER BY 1, 2";
    const TOOLS: &str = "SELECT id, msg_id FROM tool_calls ORDER BY id";

    fn all(span_end: &str) -> Span {
        Span {
            start: Utc.timestamp_opt(0, 0).unwrap(),
            end: at(span_end),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// A session in three parts. msg_2 is one response written as two records (two tool uses,
    /// one per record) that straddle the first cut; a compaction sits in the last part.
    fn session_parts() -> [Vec<String>; 3] {
        let m = "claude-sonnet-5-5";
        [
            vec![
                owner("acc-a"),
                assistant(
                    "msg_1",
                    "2026-11-10T10:00:00Z",
                    m,
                    "/w/shop",
                    (1000, 10),
                    &["t1"],
                ),
                assistant(
                    "msg_2",
                    "2026-11-10T10:01:00Z",
                    m,
                    "/w/shop",
                    (2000, 5),
                    &["t2"],
                ),
            ],
            vec![
                assistant(
                    "msg_2",
                    "2026-11-10T10:01:00Z",
                    m,
                    "/w/shop",
                    (2000, 50),
                    &["t3"],
                ),
                compact("2026-11-10T10:02:00Z"),
            ],
            vec![assistant(
                "msg_3",
                "2026-11-10T10:03:00Z",
                m,
                "/w/docs",
                (500, 100),
                &[],
            )],
        ]
    }

    /// Review B-M8: a fork (a new session) or a moved config dir (a new seat) repeats records
    /// with their own ids; a prompt and an API error are counted once, not once per copy.
    #[test]
    fn a_forked_prompt_counts_once() {
        let w = World::new(&["work", "moved"]);
        let p = serde_json::json!({"type": "user", "uuid": "rec-p1", "timestamp": "2026-11-20T10:00:00Z",
            "message": {"role": "user", "content": "Fix the VAT report"}})
        .to_string();
        let e = serde_json::json!({"type": "assistant", "uuid": "rec-e1", "isApiErrorMessage": true,
            "error": "rate_limit", "timestamp": "2026-11-20T10:01:00Z",
            "message": {"id": "x", "model": "<synthetic>", "content": [{"type": "text", "text": "API Error"}]}})
        .to_string();
        w.append("work", "s1", &[p.clone(), e.clone()]);
        w.append("work", "fork", &[p.clone(), e.clone()]);
        w.append("moved", "s1", &[p, e]);
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        assert_eq!(dump(&idx, "SELECT COUNT(*) FROM prompts"), ["1"]);
        assert_eq!(dump(&idx, "SELECT COUNT(*) FROM api_errors"), ["1"]);
    }

    /// A fork repeats its parent's records (same message, tool and record ids, same times) under
    /// a new session id, and nothing in them names the parent. The copies belong to the session
    /// that started first, whichever file is read first: read as they were written (the parent,
    /// then the fork) or rebuilt at once (the fork, the larger file, first).
    #[test]
    fn a_forks_copies_belong_to_the_session_that_started_first_in_any_read_order() {
        let rec = |v: serde_json::Value| v.to_string();
        let prompt = |id: &str, ts: &str| {
            rec(
                serde_json::json!({"type": "user", "uuid": id, "timestamp": ts,
                "message": {"role": "user", "content": "Fix the VAT report"}}),
            )
        };
        let error = |id: &str, ts: &str| {
            rec(
                serde_json::json!({"type": "assistant", "uuid": id, "isApiErrorMessage": true,
                "error": "rate_limit", "timestamp": ts,
                "message": {"id": "x", "model": "<synthetic>", "content": [{"type": "text", "text": "API Error"}]}}),
            )
        };
        // The parent started at 09:00 with a response of its own; the fork copies what came from
        // 10:00 on, then works on.
        let own = vec![
            owner("acc-a"),
            prompt("p0", "2026-11-20T09:00:00Z"),
            assistant(
                "m0",
                "2026-11-20T09:00:05Z",
                "claude-sonnet-5-5",
                "/w/shop",
                (100, 10),
                &["t0"],
            ),
        ];
        let shared = vec![
            prompt("p1", "2026-11-20T10:00:00Z"),
            assistant(
                "m1",
                "2026-11-20T10:00:05Z",
                "claude-sonnet-5-5",
                "/w/shop",
                (200, 20),
                &["t1"],
            ),
            error("e1", "2026-11-20T10:01:00Z"),
            assistant(
                "m2",
                "2026-11-20T10:02:00Z",
                "claude-sonnet-5-5",
                "/w/shop",
                (300, 30),
                &[],
            ),
        ];
        let fork_own = vec![
            prompt("p9", "2026-11-20T11:00:00Z"),
            assistant(
                "m9",
                "2026-11-20T11:00:05Z",
                "claude-sonnet-5-5",
                "/w/shop",
                (900, 90),
                &["t9"],
            ),
            assistant(
                "m10",
                "2026-11-20T11:01:00Z",
                "claude-sonnet-5-5",
                "/w/shop",
                (900, 90),
                &[],
            ),
            assistant(
                "m11",
                "2026-11-20T11:02:00Z",
                "claude-sonnet-5-5",
                "/w/shop",
                (900, 90),
                &[],
            ),
        ];
        let parent = [own, shared.clone()].concat();
        let fork = [vec![owner("acc-b")], shared, fork_own].concat();
        let full = read_in_both_orders(&parent, &fork);
        let by_session = FORK_ROWS;
        assert_eq!(
            dump(&full.1, by_session[0]),
            [
                "m0|parent|acc-a",
                "m1|parent|acc-a",
                "m10|fork|acc-b",
                "m11|fork|acc-b",
                "m2|parent|acc-a",
                "m9|fork|acc-b"
            ]
        );
        assert_eq!(
            dump(&full.1, by_session[1]),
            ["t0|parent", "t1|parent", "t9|fork"]
        );
        assert_eq!(
            dump(&full.1, by_session[2]),
            ["p0|parent", "p1|parent", "p9|fork"]
        );
        assert_eq!(dump(&full.1, by_session[3]), ["e1|parent"]);

        // A resume that copied the whole session: both start at once, the copy ends later.
        let parent = vec![
            owner("acc-a"),
            prompt("p1", "2026-11-20T10:00:00Z"),
            assistant(
                "m1",
                "2026-11-20T10:00:05Z",
                "claude-sonnet-5-5",
                "/w/shop",
                (200, 20),
                &["t1"],
            ),
        ];
        let mut fork = parent.clone();
        fork.push(assistant(
            "m9",
            "2026-11-20T11:00:05Z",
            "claude-sonnet-5-5",
            "/w/shop",
            (900, 90),
            &[],
        ));
        let full = read_in_both_orders(&parent, &fork);
        assert_eq!(
            dump(&full.1, by_session[0]),
            ["m1|parent|acc-a", "m9|fork|acc-a"]
        );
        assert_eq!(dump(&full.1, by_session[2]), ["p1|parent"]);
    }

    const FORK_ROWS: [&str; 4] = [
        "SELECT msg_id, session_id, account_uuid FROM requests ORDER BY msg_id",
        "SELECT id, session_id FROM tool_calls ORDER BY id",
        "SELECT record_id, session_id FROM prompts ORDER BY record_id",
        "SELECT record_id, session_id FROM api_errors ORDER BY record_id",
    ];

    /// Sessions `parent` and `fork` read as they were written (the parent first, the fork found
    /// later) and rebuilt at once (the fork, the larger file, first): the same rows belong to
    /// the same sessions. Returns the rebuilt index.
    fn read_in_both_orders(parent: &[String], fork: &[String]) -> (World, Index) {
        let prices = PriceBook::builtin();
        let w1 = World::new(&["work"]);
        w1.append("work", "parent", parent);
        let mut inc = w1.index("inc.sqlite");
        w1.refresh(&mut inc, &[], &prices);
        w1.append("work", "fork", fork);
        w1.refresh(&mut inc, &[], &prices);
        let w2 = World::new(&["work"]);
        w2.append("work", "parent", parent);
        w2.append("work", "fork", fork);
        let mut full = w2.index("full.sqlite");
        w2.refresh(&mut full, &[], &prices);
        for q in FORK_ROWS {
            assert_eq!(dump(&inc, q), dump(&full, q), "{q}");
        }
        (w2, full)
    }

    #[test]
    fn incremental_refresh_equals_a_full_rebuild_and_a_reread_counts_once() {
        let w = World::new(&["work"]);
        let prices = PriceBook::builtin();
        let mut inc = w.index("inc.sqlite");
        let parts = session_parts();
        for p in &parts {
            w.append("work", "s1", p);
            w.refresh(&mut inc, &[], &prices);
        }
        let mut full = w.index("full.sqlite");
        w.refresh(&mut full, &[], &prices);
        for q in [REQUESTS, SESSIONS, TOOLS] {
            assert_eq!(dump(&inc, q), dump(&full, q), "{q}");
        }
        // The file is replaced with the same content (a new inode): read again from the start,
        // tools and compactions still count once.
        w.replace("work", "s1", &parts.concat());
        w.refresh(&mut inc, &[], &prices);
        for q in [REQUESTS, SESSIONS, TOOLS] {
            assert_eq!(dump(&inc, q), dump(&full, q), "after the re-read: {q}");
        }
        let tools: i64 = inc
            .conn
            .query_row("SELECT SUM(tools) FROM requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tools, 3, "t1, t2, t3");
        let compactions: i64 = inc
            .conn
            .query_row("SELECT compactions FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(compactions, 1);
    }

    /// A usage count beyond what SQLite holds (a damaged record) is refused and counted; it
    /// does not fail the refresh, and the records around it count.
    #[test]
    fn a_usage_count_out_of_range_is_refused_not_fatal() {
        let w = World::new(&["work"]);
        let huge = serde_json::json!({
            "type": "assistant", "timestamp": "2026-11-01T10:01:00Z", "cwd": "/w/shop", "sessionId": "s",
            "message": {"id": "bad", "model": "claude-sonnet-5-5", "content": [],
                "usage": {"input_tokens": 9_223_372_036_854_775_808_u64, "output_tokens": 1}}
        })
        .to_string();
        let m = "claude-sonnet-5-5";
        w.append(
            "work",
            "s1",
            &[
                owner("acc-a"),
                assistant("a", "2026-11-01T10:00:00Z", m, "/w/shop", (10, 1), &[]),
                huge,
                assistant("b", "2026-11-01T10:02:00Z", m, "/w/shop", (10, 1), &[]),
            ],
        );
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        assert_eq!(
            dump(&idx, "SELECT msg_id FROM requests ORDER BY 1"),
            ["a", "b"]
        );
        assert_eq!(
            dump(
                &idx,
                &format!(
                    "SELECT count FROM record_types WHERE type = '{}'",
                    crate::claude::transcript::MALFORMED
                )
            ),
            ["1"]
        );
    }

    #[test]
    fn records_of_one_response_merge_by_message_id() {
        let w = World::new(&["work"]);
        let mut idx = w.index("i.sqlite");
        w.append("work", "s1", &session_parts().concat());
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let t = idx
            .total(all("2026-11-11T00:00:00Z"), &Filter::default())
            .unwrap();
        // msg_2's two records count once, with the larger output (50, not 5 + 50).
        assert_eq!(t.requests, 3);
        assert_eq!(t.usage.input, 1000 + 2000 + 500);
        assert_eq!(t.usage.output, 10 + 50 + 100);
        // Sonnet 5.5 at $2 in / $10 out per MTok: 3500 × 2 + 160 × 10 = 7000 + 1600 = 8600 µ$.
        assert!(close(t.cost, 0.0086), "{}", t.cost);
        assert_eq!((t.unpriced, t.pending), (0, 0));
        let by_project = idx
            .spend_by(
                Dim::Project,
                all("2026-11-11T00:00:00Z"),
                &Filter::default(),
            )
            .unwrap();
        // shop: msg_1 + msg_2 = 3000 × 2 + 60 × 10 = 6600 µ$; docs: 500 × 2 + 100 × 10 = 2000 µ$.
        assert_eq!(by_project[0].label, "shop");
        assert!(close(by_project[0].cost, 0.0066));
        assert_eq!(by_project[1].label, "docs");
        assert!(close(by_project[1].cost, 0.002));
    }

    /// Review H1. The transcripts' `session-owner` records say which account a request used;
    /// the seat's login history fills in for sessions without one, from the moment a change is
    /// seen (never from the profile's `profileFetchedAt`: a vault switch restores a profile
    /// fetched days earlier), and decides from then on for a session that began before it (its
    /// owner records go stale, see `a_vault_switch_under_a_running_session_moves_its_requests`).
    /// Both survive a cache rebuild.
    #[test]
    fn the_owner_decides_and_a_seen_login_change_fills_in() {
        let w = World::new(&["work"]);
        let m = "claude-sonnet-5-5";
        let req = |id: &str, ts: &str| assistant(id, ts, m, "/w/shop", (1000, 0), &[]);
        let later = |h: i64| (Utc::now() + chrono::Duration::hours(h)).to_rfc3339();
        // s1 runs under acc-a (its owner records); s0 comes from a Claude Code that wrote none.
        w.append(
            "work",
            "s1",
            &[
                owner("acc-a"),
                req("r1", "2026-11-09T10:00:00Z"),
                req("r2", "2026-11-10T10:00:00Z"),
            ],
        );
        w.append("work", "s0", &[req("r0", "2026-11-09T09:00:00Z")]);
        let seat = |acc: &str, since: &str| SeatInfo {
            seat: Default::default(),
            label: "work".into(),
            account: Some(account(acc, since)),
            switched_at: None,
            utilization: None,
        };
        let path = w.dir.path().join("i.sqlite");
        let mut idx = Index::open(&path).unwrap();
        w.refresh(
            &mut idx,
            &[seat("acc-a", "2026-11-08T00:00:00Z")],
            &PriceBook::builtin(),
        );
        // The vault switches the seat to acc-b, whose saved profile is from 11-09.
        w.refresh(
            &mut idx,
            &[seat("acc-b", "2026-11-09T00:00:00Z")],
            &PriceBook::builtin(),
        );
        let accounts = |idx: &Index| {
            dump(
                idx,
                "SELECT msg_id, account_uuid FROM requests ORDER BY msg_id",
            )
        };
        assert_eq!(accounts(&idx), ["r0|acc-a", "r1|acc-a", "r2|acc-a"]);
        let history = dump(
            &idx,
            "SELECT ts, account_uuid FROM seat_accounts ORDER BY ts",
        );
        assert_eq!(history.len(), 2);
        let switched: i64 = history[1].split('|').next().unwrap().parse().unwrap();
        assert!(
            switched >= Utc::now().timestamp_millis() - 60_000,
            "dated when seen: {history:?}"
        );
        // After the change both sessions, begun before it, follow the seat: the owner-less one,
        // and s1, whose owner record still names the account it started with.
        w.append("work", "s0", &[req("r3", &later(1))]);
        w.append("work", "s1", &[req("r4", &later(1))]);
        w.refresh(
            &mut idx,
            &[seat("acc-b", "2026-11-09T00:00:00Z")],
            &PriceBook::builtin(),
        );
        let expected = ["r0|acc-a", "r1|acc-a", "r2|acc-a", "r3|acc-b", "r4|acc-b"];
        assert_eq!(accounts(&idx), expected);

        // A new cache version: the cache is rebuilt from the transcripts. The seat no longer
        // reports any account (logged out), so only the stored login history can tell.
        drop(idx);
        Index::open(&path)
            .unwrap()
            .set_meta("schema_version", "0")
            .unwrap();
        let mut idx = Index::open(&path).unwrap();
        assert_eq!(
            idx.count(Counted::Requests).unwrap(),
            0,
            "the cache was dropped"
        );
        assert_eq!(
            idx.count(Counted::SeatAccounts).unwrap(),
            2,
            "the facts were kept"
        );
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        assert_eq!(accounts(&idx), expected);
    }

    /// Seen on Claude Code 2.1.332: the vault switches a seat while a session runs there. Claude
    /// Code reads `.credentials.json` again, so the session's next requests go out under the new
    /// login (the limits they report are the new account's), but it keeps writing `session-owner`
    /// records of the account it started with. From the switch on, dated by the vault and not
    /// when swarmy first sees the new account, the seat's login decides for that session. A
    /// session begun after the switch has owner records of its own, and they decide. An index
    /// attributed by the old rule is attributed again once.
    #[test]
    fn a_vault_switch_under_a_running_session_moves_its_requests() {
        let w = World::new(&["work"]);
        let m = "claude-sonnet-5-5";
        let req = |id: &str, ts: &str| assistant(id, ts, m, "/w/shop", (1000, 0), &[]);
        let now = Utc::now();
        let t = |min: i64| now + chrono::Duration::minutes(min);
        let ts = |min: i64| t(min).to_rfc3339();
        let seat = |acc: &str, switched_at: Option<DateTime<Utc>>| SeatInfo {
            seat: Default::default(),
            label: "work".into(),
            account: Some(account(acc, "2026-11-01T00:00:00Z")),
            utilization: None,
            switched_at,
        };
        let path = w.dir.path().join("i.sqlite");
        let mut idx = Index::open(&path).unwrap();
        // s1 begins under acc-a; swarmy sees the seat with acc-a.
        w.append("work", "s1", &[owner("acc-a"), req("r1", &ts(-60))]);
        // s3 names acc-z, which the seat never showed.
        w.append("work", "s3", &[owner("acc-z"), req("z1", &ts(-55))]);
        idx.set_now(t(-50));
        w.refresh(&mut idx, &[seat("acc-a", None)], &PriceBook::builtin());
        // A row naming the account the seat already had (an older swarmy's migration left such
        // pairs) is no change of login: s3's owner keeps deciding.
        idx.conn
            .execute(
                "INSERT INTO seat_accounts(seat, ts, account_uuid) VALUES (?1, ?2, 'acc-a')",
                rusqlite::params![w.id("work"), t(-45).timestamp_millis()],
            )
            .unwrap();
        w.append("work", "s3", &[req("z2", &ts(-42))]);

        // The vault switches the seat to acc-b 30 minutes ago; the next pass that sees it runs
        // now. s1 went on working and still names acc-a; s2 began after the switch.
        w.append(
            "work",
            "s1",
            &[
                req("r2", &ts(-40)),
                owner("acc-a"),
                req("r3", &ts(-20)),
                owner("acc-a"),
                req("r4", &ts(-5)),
            ],
        );
        w.append("work", "s2", &[owner("acc-c"), req("r5", &ts(-10))]);
        idx.set_now(now);
        w.refresh(
            &mut idx,
            &[seat("acc-b", Some(t(-30)))],
            &PriceBook::builtin(),
        );
        let accounts = |idx: &Index| {
            dump(
                idx,
                "SELECT msg_id, account_uuid FROM requests ORDER BY msg_id",
            )
        };
        let expected = [
            "r1|acc-a", "r2|acc-a", "r3|acc-b", "r4|acc-b", "r5|acc-c", "z1|acc-z", "z2|acc-z",
        ];
        assert_eq!(accounts(&idx), expected);
        assert_eq!(
            dump(
                &idx,
                "SELECT ts, account_uuid FROM seat_accounts ORDER BY ts"
            ),
            [
                format!("{}|acc-a", t(-50).timestamp_millis()),
                format!("{}|acc-a", t(-45).timestamp_millis()),
                format!("{}|acc-b", t(-30).timestamp_millis()),
            ],
            "the change is dated by the switch"
        );

        // An index attributed by the old rule (the owner decided for r3 and r4): opened by this
        // swarmy, its next refresh attributes them again, once.
        idx.conn
            .execute_batch(
                "UPDATE requests SET account_uuid = 'acc-a' WHERE msg_id IN ('r3', 'r4');
                 UPDATE meta SET value = '1' WHERE key = 'owner_rule_rev';",
            )
            .unwrap();
        drop(idx);
        let mut idx = Index::open(&path).unwrap();
        w.refresh(
            &mut idx,
            &[seat("acc-b", Some(t(-30)))],
            &PriceBook::builtin(),
        );
        assert_eq!(accounts(&idx), expected);
        assert_eq!(
            idx.meta(super::super::accounts::OWNERS_PENDING).unwrap(),
            None
        );
        assert_eq!(
            idx.meta("owner_rule_rev").unwrap().as_deref(),
            Some(super::super::OWNER_RULE_REV)
        );
    }

    #[test]
    fn a_cache_rebuild_keeps_agent_types_and_session_titles_of_deleted_transcripts() {
        // Claude Code deletes transcripts after 30 days; a cache rebuild reads the transcripts
        // again and must not forget what only the dimension tables still know.
        let w = World::new(&["work"]);
        let m = "claude-sonnet-5-5";
        for (session, cwd, title) in [
            ("old", "/w/shop", "old work"),
            ("new", "/w/docs", "new work"),
        ] {
            w.append(
                "work",
                session,
                &[
                    serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": "s"}).to_string(),
                    assistant(&format!("{session}-r1"), "2026-11-10T10:00:00Z", m, cwd, (1000, 10), &[]),
                    compact("2026-11-10T10:01:00Z"),
                    serde_json::json!({"type": "cost-state", "totalCostUSD": 0.5, "modelUsage": {}}).to_string(),
                ],
            );
            let agent = format!("{session}-a");
            w.write_subagent(
                "work",
                session,
                &agent,
                &[assistant(
                    &format!("{session}-r2"),
                    "2026-11-10T10:02:00Z",
                    m,
                    cwd,
                    (100, 1),
                    &[],
                )],
            );
            std::fs::write(
                w.subagent("work", session, &agent)
                    .with_extension("meta.json"),
                r#"{"agentType":"reviewer","description":"check the totals","finished":true}"#,
            )
            .unwrap();
        }
        let path = w.dir.path().join("i.sqlite");
        let mut idx = Index::open(&path).unwrap();
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let dims = |idx: &Index| -> Vec<Vec<String>> {
            crate::index::schema::DIMENSION_TABLES
                .iter()
                .map(|t| dump(idx, &format!("SELECT * FROM {t} ORDER BY 1, 2, 3")))
                .collect()
        };
        let before = dims(&idx);
        assert_eq!(before[0].len(), 2, "two sessions");
        assert_eq!(before[1].len(), 4, "two main threads, two subagents");
        // The old session's transcripts are deleted, then the cache version changes.
        std::fs::remove_file(w.transcript("work", "old")).unwrap();
        std::fs::remove_dir_all(w.transcript("work", "old").with_extension("")).unwrap();
        drop(idx);
        Index::open(&path)
            .unwrap()
            .set_meta("schema_version", "6")
            .unwrap();
        let mut idx = Index::open(&path).unwrap();
        assert_eq!(
            idx.count(Counted::Requests).unwrap(),
            2,
            "the raw rows were dropped, but for those of the deleted transcripts (nothing else has them)"
        );
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        // The new session is read again and merges into its rows unchanged; the old one keeps
        // its title, compactions, agent type and project although nothing on disk says so.
        assert_eq!(dims(&idx), before);
        let types = idx
            .spend_by(
                Dim::AgentType,
                all("2026-11-11T00:00:00Z"),
                &Filter::default(),
            )
            .unwrap();
        assert_eq!(
            types
                .iter()
                .map(|r| (r.key.as_str(), r.requests))
                .collect::<Vec<_>>(),
            vec![("main thread", 2), ("reviewer", 2)],
            "the deleted session's requests still count"
        );
    }

    fn cache_book(input: f64, output: f64) -> PriceBook {
        let mut b = PriceBook::builtin();
        let cache = serde_json::json!({"claude-sonnet-5-5": {"inputTokens": input, "outputTokens": output}});
        b.merge_claude_code_cache(&"work".into(), &cache);
        b
    }

    #[test]
    fn a_price_change_reprices_the_index_but_not_the_past() {
        let w = World::new(&["work"]);
        let m = "claude-sonnet-5-5";
        // One million input tokens each, at noon UTC, days apart (any time zone keeps the day).
        w.append(
            "work",
            "s1",
            &[
                assistant(
                    "a",
                    "2026-11-02T12:00:00Z",
                    m,
                    "/w/shop",
                    (1_000_000, 0),
                    &[],
                ),
                assistant(
                    "b",
                    "2026-11-08T12:00:00Z",
                    m,
                    "/w/shop",
                    (1_000_000, 0),
                    &[],
                ),
                assistant(
                    "c",
                    "2026-11-14T12:00:00Z",
                    m,
                    "/w/shop",
                    (1_000_000, 0),
                    &[],
                ),
                assistant(
                    "u",
                    "2026-11-14T12:00:00Z",
                    "claude-unknown-9",
                    "/w/shop",
                    (10, 0),
                    &[],
                ),
            ],
        );
        let mut idx = w.index("i.sqlite");
        let cost = |idx: &Index, day: &str| {
            idx.total(
                Span {
                    start: at(&format!("{day}T00:00:00Z")),
                    end: at(&format!("{day}T23:59:59Z")),
                },
                &Filter {
                    model: Some("sonnet".into()),
                    ..Default::default()
                },
            )
            .unwrap()
            .cost
        };
        // Built-in prices: $2 per MTok of input, so $2 each.
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        for d in ["2026-11-02", "2026-11-08", "2026-11-14"] {
            assert!(close(cost(&idx, d), 2.0), "{d}");
        }
        // The unknown model is counted as unpriced, never as $0 inside the cost.
        let t = idx
            .total(all("2026-12-01T00:00:00Z"), &Filter::default())
            .unwrap();
        assert_eq!((t.requests, t.unpriced, t.pending), (4, 1, 0));
        assert!(close(t.cost, 6.0));

        // Claude Code's cache shows $3 on 11-05, then $4 on 11-11.
        let v1 = cache_book(3.0, 15.0);
        idx.dated_prices(&v1, at("2026-11-05T12:00:00Z")).unwrap();
        idx.price_all(&v1).unwrap();
        // Before the first sighting the built-in price stays; from 11-05 the cache's.
        assert!(close(cost(&idx, "2026-11-02"), 2.0));
        assert!(close(cost(&idx, "2026-11-08"), 3.0));
        assert!(close(cost(&idx, "2026-11-14"), 3.0));
        let v2 = cache_book(4.0, 20.0);
        idx.dated_prices(&v2, at("2026-11-11T12:00:00Z")).unwrap();
        idx.price_all(&v2).unwrap();
        assert!(close(cost(&idx, "2026-11-02"), 2.0));
        assert!(
            close(cost(&idx, "2026-11-08"), 3.0),
            "the past keeps its price"
        );
        assert!(close(cost(&idx, "2026-11-14"), 4.0));
        // The fingerprint the costs were computed with is the one read with them.
        let (row, fp) = idx
            .total_priced(all("2026-12-01T00:00:00Z"), &Filter::default())
            .unwrap();
        assert_eq!(fp, idx.meta("price_fp").unwrap());
        assert!(close(row.cost, 2.0 + 3.0 + 4.0));

        // Another process (or a restart) dates the same cache the same way: same costs, no
        // reprice back and forth.
        let fp_before = idx.meta("price_fp").unwrap();
        drop(idx);
        let mut again = w.index("i.sqlite");
        again.price_all(&v2).unwrap();
        assert_eq!(again.meta("price_fp").unwrap(), fp_before);
        assert!(close(cost(&again, "2026-11-08"), 3.0));

        // A user override reprices everything it covers, keeping fast mode and cache ratios.
        let mut user = cache_book(4.0, 20.0);
        user.push_override(
            m,
            None,
            1.0,
            5.0,
            &RateOverrides::default(),
            at("2026-11-22T12:00:00Z").date_naive(),
        )
        .unwrap();
        again.price_all(&user).unwrap();
        for d in ["2026-11-02", "2026-11-08", "2026-11-14"] {
            assert!(close(cost(&again, d), 1.0), "{d}");
        }
    }

    #[test]
    fn the_first_sighting_of_an_unknown_model_prices_its_whole_history() {
        // A model newer than the built-in table: only Claude Code's cache prices it, so its first
        // version applies before the day it was seen too.
        let w = World::new(&["work"]);
        w.append(
            "work",
            "s1",
            &[assistant(
                "n",
                "2026-10-01T12:00:00Z",
                "claude-new-9",
                "/w/shop",
                (1_000_000, 0),
                &[],
            )],
        );
        let mut idx = w.index("i.sqlite");
        let mut book = PriceBook::builtin();
        book.merge_claude_code_cache(
            &"work".into(),
            &serde_json::json!({"claude-new-9": {"inputTokens": 7, "outputTokens": 35}}),
        );
        let dated = idx.dated_prices(&book, at("2026-11-20T12:00:00Z")).unwrap();
        let e: &PriceEntry = dated
            .entries()
            .iter()
            .find(|e| e.model == "claude-new-9")
            .unwrap();
        assert_eq!(e.from, None);
        assert_eq!(e.source, PriceSource::ClaudeCode("work".into()));
        let expected = Rates::derive(7.0, 35.0, &RateOverrides::default(), None);
        assert_eq!(e.rates, expected);
        w.refresh(&mut idx, &[], &book);
        let t = idx
            .total(all("2026-12-01T00:00:00Z"), &Filter::default())
            .unwrap();
        // 1M input at $7.
        assert!(close(t.cost, 7.0), "{}", t.cost);
        assert_eq!(t.unpriced, 0);
    }

    #[test]
    fn files_edited_and_edit_conflicts_count_the_same_tools() {
        // Two sessions edit the same notebook in the same hour (NotebookEdit), and the same
        // source file (Edit, Write); one of them also reads a file, which is no edit.
        let w = World::new(&["work"]);
        let nb = serde_json::json!({"notebook_path": "/w/shop/a.ipynb"});
        let src = serde_json::json!({"file_path": "/w/shop/main.rs"});
        let other = serde_json::json!({"file_path": "/w/shop/lib.rs"});
        w.append(
            "work",
            "s1",
            &[
                tool_calls(
                    "e1",
                    "2026-11-10T10:05:00Z",
                    "/w/shop",
                    &[
                        ("t1", "NotebookEdit", nb.clone()),
                        ("t2", "Edit", src.clone()),
                    ],
                ),
                tool_calls(
                    "e2",
                    "2026-11-10T10:06:00Z",
                    "/w/shop",
                    &[("t3", "Read", other)],
                ),
            ],
        );
        w.append(
            "work",
            "s2",
            &[tool_calls(
                "e3",
                "2026-11-10T10:20:00Z",
                "/w/shop",
                &[("t4", "NotebookEdit", nb), ("t5", "Write", src)],
            )],
        );
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let span = all("2026-11-11T00:00:00Z");
        let e = idx.efficiency(span).unwrap();
        // Two files, four edits; the read is not an edit.
        assert_eq!((e.files_edited, e.edits), (2, 4));
        let mut conflicts = idx.edit_conflicts(at("2026-11-10T00:00:00Z")).unwrap();
        conflicts.sort();
        assert_eq!(
            conflicts,
            vec![
                ("/w/shop/a.ipynb".to_string(), 2),
                ("/w/shop/main.rs".to_string(), 2)
            ],
            "every file counted as edited can be a conflict, notebooks too"
        );
    }

    #[test]
    fn the_efficiency_split_adds_up_to_the_cost() {
        // A fast-mode Opus 5.5 request with web searches and a long-context Haiku 5.5 request:
        // the multipliers the old split left out.
        let w = World::new(&["work"]);
        let rec = |id: &str, model: &str, speed: &str, input: u64, cread: u64, web: u64| {
            serde_json::json!({
                "type": "assistant", "timestamp": "2026-11-10T10:00:00Z", "cwd": "/w/shop", "sessionId": "s",
                "message": {"id": id, "model": model, "content": [],
                    "usage": {"input_tokens": input, "output_tokens": 4000,
                              "cache_creation_input_tokens": 0, "cache_read_input_tokens": cread,
                              "server_tool_use": {"web_search_requests": web}, "speed": speed}}
            })
            .to_string()
        };
        w.append(
            "work",
            "s1",
            &[
                rec("o", "claude-opus-5-5", "fast", 300_000, 100_000, 3),
                rec("h", "claude-haiku-5-5", "standard", 300_000, 100_000, 0),
            ],
        );
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let e = idx.efficiency(all("2026-11-11T00:00:00Z")).unwrap();
        // Opus fast (×2): input 300k × $4 = 1.20 → 2.40; reads 100k × $0.20 = 0.02 → 0.04;
        // output 4k × $20 = 0.08 → 0.16; web 3 × $0.01 → 0.06. Total 2.66.
        // Haiku long context (430k > 100k, ×5): input 300k × $0.10 × 5 = 0.15; reads 100k ×
        // $0.01 × 5 = 0.005; output 4k × $0.50 × 5 = 0.01. Total 0.165.
        assert!(close(e.cost, 2.66 + 0.165), "{}", e.cost);
        assert!(
            close(e.split.total(), e.cost),
            "{:?} vs {}",
            e.split,
            e.cost
        );
        assert!(close(e.split.input, 2.4 + 0.15));
        assert!(close(e.split.cache_read, 0.04 + 0.005));
        assert!(close(e.split.output, 0.16 + 0.01));
        assert!(close(e.split.web, 0.06));
        // Saved by reading the cache instead of paying input: Opus 100k × (4 − 0.2) × 2 = 0.76,
        // Haiku 100k × (0.10 − 0.01) × 5 = 0.045.
        assert!(close(e.cache_saved, 0.76 + 0.045), "{}", e.cache_saved);
        // A reprice moves the split with the cost.
        let mut user = PriceBook::builtin();
        user.push_override(
            "claude-opus-5-5",
            None,
            1.0,
            5.0,
            &RateOverrides::default(),
            at("2026-11-22T12:00:00Z").date_naive(),
        )
        .unwrap();
        idx.price_all(&user).unwrap();
        let e = idx.efficiency(all("2026-11-11T00:00:00Z")).unwrap();
        assert!(
            close(e.split.total(), e.cost),
            "{:?} vs {}",
            e.split,
            e.cost
        );
    }

    /// What the EFFICIENCY panel and `doctor` count, besides the requests.
    const COUNTED: &[&str] = &[
        "SELECT seat, session_id, ts, chars FROM prompts ORDER BY 1, 2, 3",
        "SELECT seat, session_id, agent_id, ts, kind FROM api_errors ORDER BY 1, 2, 3, 4",
        "SELECT seat, session_id, compactions FROM sessions ORDER BY 1, 2",
        "SELECT type, SUM(count) FROM record_types GROUP BY 1 ORDER BY 1",
        "SELECT msg_id, tools FROM requests ORDER BY 1",
    ];

    /// A main transcript with prompts, an API error, a compaction and tool calls, and one
    /// subagent with its own error and tool call.
    fn busy_session(w: &World) {
        let m = "claude-sonnet-5-5";
        w.append(
            "work",
            "s1",
            &[
                owner("acc-a"),
                prompt("2026-11-10T09:59:00Z", "Fix the report"),
                assistant(
                    "r1",
                    "2026-11-10T10:00:00Z",
                    m,
                    "/w/shop",
                    (1000, 10),
                    &["t1", "t2"],
                ),
                api_error("2026-11-10T10:01:00Z"),
                compact("2026-11-10T10:02:00Z"),
                prompt("2026-11-10T10:03:00Z", "And the totals"),
                assistant(
                    "r2",
                    "2026-11-10T10:04:00Z",
                    m,
                    "/w/shop",
                    (500, 5),
                    &["t3"],
                ),
            ],
        );
        w.write_subagent(
            "work",
            "s1",
            "a1",
            &[
                assistant(
                    "sa1",
                    "2026-11-10T10:00:30Z",
                    m,
                    "/w/shop",
                    (200, 2),
                    &["t9"],
                ),
                api_error("2026-11-10T10:00:40Z"),
            ],
        );
    }

    fn counted(idx: &Index) -> Vec<Vec<String>> {
        COUNTED.iter().map(|q| dump(idx, q)).collect()
    }

    #[test]
    fn two_processes_refreshing_at_once_count_everything_once() {
        let w = World::new(&["work"]);
        busy_session(&w);
        let prices = PriceBook::builtin();
        let mut reference = w.index("ref.sqlite");
        w.refresh(&mut reference, &[], &prices);
        let want = counted(&reference);
        // Sanity: the fixture counts what it should.
        assert_eq!(want[0].len(), 2, "two prompts");
        assert_eq!(want[1].len(), 2, "two API errors");
        assert_eq!(
            want[2],
            vec![format!("{}|s1|1", w.id("work"))],
            "one compaction"
        );
        assert_eq!(
            want[4],
            ["r1|2", "r2|1", "sa1|1"].map(String::from).to_vec()
        );

        // Process A plans the work (reads the offsets), process B refreshes the same index
        // completely, then A writes what it read: A's work is dropped, nothing counts twice.
        let mut a = w.index("shared.sqlite");
        let mut b = w.index("shared.sqlite");
        let (work, _) = a.plan(&w.files(), &w.seats).unwrap();
        assert_eq!(work.len(), 2);
        w.refresh(&mut b, &[], &prices);
        let mut resolver = Resolver::new(Vec::new());
        a.ingest(work, &[], &mut resolver, &prices, false, &mut |_| {})
            .unwrap();
        assert_eq!(counted(&a), want);
        assert_eq!(dump(&a, REQUESTS), dump(&reference, REQUESTS));

        // More lines arrive; both refresh again, in turn and interleaved: still once.
        w.append("work", "s1", &[prompt("2026-11-10T10:05:00Z", "Thanks")]);
        w.append("work", "s1", &[compact("2026-11-10T10:06:00Z")]);
        let (work, _) = a.plan(&w.files(), &w.seats).unwrap();
        w.refresh(&mut b, &[], &prices);
        a.ingest(work, &[], &mut resolver, &prices, false, &mut |_| {})
            .unwrap();
        w.refresh(&mut a, &[], &prices);
        w.refresh(&mut reference, &[], &prices);
        assert_eq!(counted(&a), counted(&reference));
        assert_eq!(dump(&a, "SELECT compactions FROM sessions"), vec!["2"]);
        assert_eq!(dump(&a, "SELECT COUNT(*) FROM prompts"), vec!["3"]);
    }

    /// Claude Code writes `"finished": true` into an agent's `meta.json` a moment after the
    /// agent's last transcript line. A refresh that ran between the two read the whole
    /// transcript with the old meta; the transcript never changes again, so the agent stayed
    /// unfinished in the index and showed as stalled for a day.
    #[test]
    fn a_meta_written_after_the_last_transcript_line_still_finishes_the_agent() {
        let w = World::new(&["work"]);
        let (m, cwd) = ("claude-opus-4-8", "/w/shop");
        w.append(
            "work",
            "s1",
            &[assistant(
                "r1",
                "2026-11-10T10:00:00Z",
                m,
                cwd,
                (100, 1),
                &[],
            )],
        );
        let meta = |agent: &str| w.subagent("work", "s1", agent).with_extension("meta.json");
        for agent in ["late", "running"] {
            // The meta as written at the spawn, then the agent's lines.
            std::fs::write(meta(agent), r#"{"agentType":"reviewer"}"#).unwrap();
            w.write_subagent(
                "work",
                "s1",
                agent,
                &[assistant(
                    &format!("{agent}-r1"),
                    "2026-11-10T10:01:00Z",
                    m,
                    cwd,
                    (100, 1),
                    &[],
                )],
            );
        }
        let mut idx = w.index("i.sqlite");
        let prices = PriceBook::builtin();
        // The refresh between the last line and the meta: both agents are at work.
        w.refresh(&mut idx, &[], &prices);
        let finished = |idx: &Index| {
            dump(
                idx,
                "SELECT agent_id, finished FROM agents WHERE agent_id != '' ORDER BY 1",
            )
        };
        assert_eq!(finished(&idx), ["late|0", "running|0"]);
        let before = counted(&idx);
        let seq = |idx: &Index, agent: &str| {
            idx.agent_seq(crate::index::live::AgentKey {
                seat: &w.id("work"),
                session_id: &crate::core::ids::SessionId::from("s1"),
                agent_id: agent,
            })
        };
        let (late_seq, running_seq) = (seq(&idx, "late"), seq(&idx, "running"));

        // Claude Code marks one of them finished; its transcript stays as it is.
        std::fs::write(meta("late"), r#"{"agentType":"reviewer","finished":true}"#).unwrap();
        w.refresh(&mut idx, &[], &prices);
        assert_eq!(finished(&idx), ["late|1", "running|0"]);
        // Nothing was read twice, and only the finished agent counts as changed.
        assert_eq!(counted(&idx), before);
        assert!(seq(&idx, "late") > late_seq);
        assert_eq!(seq(&idx, "running"), running_seq);
        // The hot refresh (the dashboard's, every second) does the same.
        std::fs::write(
            meta("running"),
            r#"{"agentType":"reviewer","finished":true}"#,
        )
        .unwrap();
        let mut resolver = Resolver::new(vec![]);
        let live = [(w.id("work"), "s1".to_string())];
        idx.refresh_hot(&w.seats, &live, &[], &mut resolver, &prices)
            .unwrap();
        assert_eq!(finished(&idx), ["late|1", "running|1"]);
        assert_eq!(counted(&idx), before);
    }

    /// Claude Code never writes `"finished"` into a workflow agent's `meta.json`: what ends the
    /// run's agents is the run's record, `<session>/workflows/<run>.json`. Without it every
    /// agent of every workflow showed as stalled from 20 minutes after the run to a day later.
    #[test]
    fn a_workflow_runs_record_finishes_its_agents() {
        let w = World::new(&["work"]);
        let (m, cwd) = ("claude-opus-4-8", "/w/shop");
        w.append(
            "work",
            "s1",
            &[assistant(
                "r1",
                "2026-11-10T10:00:00Z",
                m,
                cwd,
                (100, 1),
                &[],
            )],
        );
        let session = w
            .subagent("work", "s1", "x")
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let agent = |run: &str, agent: &str| {
            let dir = session.join("subagents/workflows").join(run);
            std::fs::create_dir_all(&dir).unwrap();
            // The meta of a workflow agent, as Claude Code leaves it for good.
            std::fs::write(
                dir.join(format!("agent-{agent}.meta.json")),
                r#"{"agentType":"reviewer","inWorkflowRun":true}"#,
            )
            .unwrap();
            let line = assistant(
                &format!("{agent}-r1"),
                "2026-11-10T10:01:00Z",
                m,
                cwd,
                (100, 1),
                &[],
            );
            std::fs::write(
                dir.join(format!("agent-{agent}.jsonl")),
                format!("{line}\n"),
            )
            .unwrap();
        };
        let record = |run: &str, status: &str| {
            std::fs::create_dir_all(session.join("workflows")).unwrap();
            std::fs::write(
                session.join("workflows").join(format!("{run}.json")),
                format!(r#"{{"runId":"{run}","agentCount":1,"status":"{status}"}}"#),
            )
            .unwrap();
        };
        // One run ended before the index ever saw it, two are at work.
        agent("wf_done", "a");
        record("wf_done", "completed");
        agent("wf_live", "b");
        agent("wf_live", "c");
        agent("wf_dies", "d");
        let mut idx = w.index("i.sqlite");
        let prices = PriceBook::builtin();
        w.refresh(&mut idx, &[], &prices);
        let finished = |idx: &Index| {
            dump(
                idx,
                "SELECT agent_id, finished FROM agents WHERE kind = 'workflow' ORDER BY 1",
            )
        };
        assert_eq!(finished(&idx), ["a|1", "b|0", "c|0", "d|0"]);
        let before = counted(&idx);

        // A run ends, another is killed: their agents' transcripts stay as they are.
        record("wf_live", "completed");
        record("wf_dies", "killed");
        let mut resolver = Resolver::new(vec![]);
        let live = [(w.id("work"), "s1".to_string())];
        idx.refresh_hot(&w.seats, &live, &[], &mut resolver, &prices)
            .unwrap();
        assert_eq!(finished(&idx), ["a|1", "b|1", "c|1", "d|1"]);
        assert_eq!(counted(&idx), before);
    }

    /// Claude Code does not mark an agent it stopped (`TaskStop`, Esc): its `meta.json` never
    /// says finished. The transcript does: its last record is the interruption.
    #[test]
    fn an_agent_that_was_stopped_is_finished_until_it_works_again() {
        let w = World::new(&["work"]);
        let (m, cwd) = ("claude-opus-4-8", "/w/shop");
        w.append(
            "work",
            "s1",
            &[assistant(
                "r1",
                "2026-11-10T10:00:00Z",
                m,
                cwd,
                (100, 1),
                &[],
            )],
        );
        let work = |n: u32| {
            assistant(
                &format!("a-r{n}"),
                &format!("2026-11-10T10:0{n}:00Z"),
                m,
                cwd,
                (100, 1),
                &[],
            )
        };
        let stopped = prompt(
            "2026-11-10T10:02:00Z",
            "[Request interrupted by user for tool use]",
        );
        std::fs::write(
            w.subagent("work", "s1", "a").with_extension("meta.json"),
            r#"{"agentType":"reviewer"}"#,
        )
        .unwrap();
        // Appended, as Claude Code writes: each refresh reads only the new lines.
        let add = |line: String| {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(w.subagent("work", "s1", "a"))
                .unwrap();
            writeln!(f, "{line}").unwrap();
        };
        add(work(1));
        let mut idx = w.index("i.sqlite");
        let prices = PriceBook::builtin();
        let finished = |idx: &Index| dump(idx, "SELECT finished FROM agents WHERE agent_id = 'a'");
        w.refresh(&mut idx, &[], &prices);
        assert_eq!(finished(&idx), ["0"]);

        // Stopped: the interruption is the last record.
        add(stopped);
        w.refresh(&mut idx, &[], &prices);
        assert_eq!(finished(&idx), ["1"]);
        // A record that is no activity after it changes nothing.
        add(r#"{"type":"agent-color","agentColor":"blue"}"#.to_string());
        w.refresh(&mut idx, &[], &prices);
        assert_eq!(finished(&idx), ["1"]);
        // Sent a message, it works again.
        add(work(3));
        w.refresh(&mut idx, &[], &prices);
        assert_eq!(finished(&idx), ["0"]);
        // Read again from the start, the last record decides: at work.
        let mut again = w.index("again.sqlite");
        w.refresh(&mut again, &[], &prices);
        assert_eq!(finished(&again), ["0"]);
        // The interruption is no prompt of the user's.
        assert_eq!(dump(&idx, "SELECT COUNT(*) FROM prompts"), ["0"]);
    }

    #[test]
    fn rereading_files_changes_no_count_and_a_subagent_reset_keeps_the_main_rows() {
        let w = World::new(&["work"]);
        busy_session(&w);
        let prices = PriceBook::builtin();
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &prices);
        let want = counted(&idx);
        // A second connection over the same index finds nothing new.
        let mut other = w.index("i.sqlite");
        w.refresh(&mut other, &[], &prices);
        assert_eq!(counted(&other), want);

        // The subagent's file is replaced with the same content (read again from the start):
        // the main session's prompts and compactions stay, nothing doubles.
        let p = w.subagent("work", "s1", "a1");
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<String> = text.lines().map(String::from).collect();
        w.write_subagent("work", "s1", "a1", &lines);
        w.refresh(&mut idx, &[], &prices);
        assert_eq!(counted(&idx), want, "after the subagent's re-read");

        // The subagent's file shrinks to its first line: only its own error goes.
        w.write_subagent("work", "s1", "a1", &lines[..1]);
        w.refresh(&mut idx, &[], &prices);
        let after = counted(&idx);
        assert_eq!(after[0], want[0], "the main session's prompts are kept");
        assert_eq!(after[2], want[2], "and its compaction");
        assert_eq!(
            after[1],
            vec![format!("{}|s1||1794304860000|rate_limit", w.id("work"))],
            "the main thread's error (10:01) stays, the subagent's is gone"
        );

        // The main file replaced with the same content: same counts as before.
        let main = std::fs::read_to_string(w.transcript("work", "s1")).unwrap();
        let lines: Vec<String> = main.lines().map(String::from).collect();
        w.replace("work", "s1", &lines);
        w.refresh(&mut idx, &[], &prices);
        assert_eq!(counted(&idx), after);
    }

    #[test]
    fn repeated_readings_are_stored_once_every_few_minutes() {
        let w = World::new(&["work", "home"]);
        let resets = |jitter: &str| format!("2026-11-25T{jitter}Z");
        let seat = |label: &str, used: f64, fetched: &str, reset: &str| SeatInfo {
            seat: Default::default(),
            label: label.into(),
            account: Some(account("acc-a", "2026-11-01T00:00:00Z")),
            switched_at: None,
            utilization: Some(weekly("acc-a", fetched, used, reset)),
        };
        let mut idx = w.index("i.sqlite");
        let book = PriceBook::builtin();
        // The same 40% every 10 s for 12 minutes, from two seats of one account whose resets
        // differ by jitter; then 41%.
        for i in 0..72 {
            let t = at("2026-11-22T10:00:00Z") + chrono::Duration::seconds(10 * i);
            let f = t.to_rfc3339();
            w.refresh(
                &mut idx,
                &[
                    seat("work", 40.0, &f, &resets("14:59:59.56")),
                    seat("home", 40.0, &f, &resets("15:00:00.13")),
                ],
                &book,
            );
        }
        w.refresh(
            &mut idx,
            &[seat(
                "work",
                41.0,
                "2026-11-22T10:12:05Z",
                &resets("15:00:00"),
            )],
            &book,
        );
        let rows = dump(
            &idx,
            "SELECT fetched_at, utilization FROM readings ORDER BY fetched_at",
        );
        // 40% at 10:00, again at 10:05:10 and 10:10:20 (more than 5 minutes after the last one
        // stored), and 41% at once.
        let ms = |t: &str| at(t).timestamp_millis();
        assert_eq!(
            rows,
            vec![
                format!("{}|40.000000000", ms("2026-11-22T10:00:00Z")),
                format!("{}|40.000000000", ms("2026-11-22T10:05:10Z")),
                format!("{}|40.000000000", ms("2026-11-22T10:10:20Z")),
                format!("{}|41.000000000", ms("2026-11-22T10:12:05Z")),
            ]
        );
    }

    #[test]
    fn a_price_edit_applies_without_new_transcript_data_and_never_reads_as_zero() {
        // Review H8: no −1 sentinel (the schema refuses it, see `schema::tests`), and a
        // `[[price]]` edit reprices at the next refresh even when no file changed.
        let w = World::new(&["work"]);
        let m = "claude-sonnet-5-5";
        w.append(
            "work",
            "s1",
            &[assistant(
                "a",
                "2026-11-10T12:00:00Z",
                m,
                "/w/shop",
                (1_000_000, 0),
                &[],
            )],
        );
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &PriceBook::builtin());
        let total = |idx: &Index| {
            idx.total(all("2026-12-01T00:00:00Z"), &Filter::default())
                .unwrap()
        };
        // 1M input at the built-in $2.
        assert!(close(total(&idx).cost, 2.0));
        let mut user = PriceBook::builtin();
        user.push_override(
            m,
            None,
            5.0,
            25.0,
            &RateOverrides::default(),
            at("2026-11-22T12:00:00Z").date_naive(),
        )
        .unwrap();
        w.refresh(&mut idx, &[], &user);
        let t = total(&idx);
        // 1M input at the user's $5, priced: nothing pending, nothing negative.
        assert!(close(t.cost, 5.0), "{}", t.cost);
        assert_eq!((t.pending, t.unpriced), (0, 0));
        assert_eq!(
            dump(
                &idx,
                "SELECT COUNT(*) FROM requests WHERE cost < 0 OR priced IS NULL"
            ),
            vec!["0"]
        );
    }
}
