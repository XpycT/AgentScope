//! OMP metadata discovery, tree identity and native relationship parsing: the
//! bounded probe, the title slot, header evidence, explicit parent/fork/
//! continuation links and the active-leaf index. No ProviderReader exists yet;
//! these tests drive the concrete parsing modules and the catalog.

mod support;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentscope::domain::{
    AggregationClass, RefreshBudget, SessionKey, SessionLink, SessionMetadata, SourceKey, Warning,
    WarningKind,
};
use agentscope::perf;
use agentscope::providers::omp::links::resolve_omp_links;
use agentscope::providers::omp::metadata::{
    parse_header, parse_title_slot, parse_title_slot_entry,
};
use agentscope::providers::omp::tree::parse_entry_header;
use agentscope::providers::omp::{
    ActiveLeaf, OmpCatalog, OmpEntryHeader, OmpHeader, OmpInsert, OmpTree, OmpTreeError,
};
use serde_json::{Value, json};
use support::omp as fx;
use support::set_modified;

const REJ: &str = agentscope::providers::omp::links::REJECTED_CONTINUATION;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

/// Everything a run of slices reported.
#[derive(Default)]
struct Found {
    /// Latest report per key, in first-report order.
    latest: HashMap<SessionKey, SessionMetadata>,
    order: Vec<SessionKey>,
    removed: Vec<SessionKey>,
    invalidated: Vec<SessionKey>,
    warnings: Vec<Warning>,
    slices: usize,
    reported: usize,
}

impl Found {
    fn by_id(&self, id: &str) -> &SessionMetadata {
        let matches: Vec<&SessionMetadata> = self
            .latest
            .values()
            .filter(|m| m.key.native_id == id)
            .collect();
        assert_eq!(matches.len(), 1, "exactly one session with id {id}");
        matches[0]
    }

    fn has_warning(&self, kind: WarningKind, needle: &str) -> bool {
        self.warnings
            .iter()
            .any(|w| w.key.kind == kind && w.key.source.contains(needle))
    }
}

fn slice(catalog: &mut OmpCatalog, budget: RefreshBudget, found: &mut Found) -> bool {
    let delta = catalog.step(budget).expect("step");
    found.slices += 1;
    for meta in delta.sessions {
        found.reported += 1;
        if !found.latest.contains_key(&meta.key) {
            found.order.push(meta.key.clone());
        }
        found.latest.insert(meta.key.clone(), meta);
    }
    for key in delta.removed {
        found.latest.remove(&key);
        found.removed.push(key);
    }
    found.invalidated.extend(delta.invalidated);
    found.warnings.extend(delta.warnings);
    delta.scan.done
}

/// Runs slices to the end of one pass.
fn pass(catalog: &mut OmpCatalog, budget: RefreshBudget, found: &mut Found) {
    for _ in 0..20_000 {
        if slice(catalog, budget, found) {
            return;
        }
    }
    panic!("discovery never finished");
}

fn discover(root: &Path) -> Found {
    let mut catalog = OmpCatalog::new(vec![root.to_path_buf()], 1000, 64 << 20);
    let mut found = Found::default();
    pass(&mut catalog, BIG, &mut found);
    found
}

// Lookups by path need the discovered key; the helper above cannot know the id,
// so path lookups go through the stored sources.
trait ByPath {
    fn path(&self, path: &Path) -> &SessionMetadata;
}

impl ByPath for Found {
    fn path(&self, path: &Path) -> &SessionMetadata {
        self.latest
            .values()
            .find(|m| matches!(&m.key.source, SourceKey::File(p) if p == path))
            .unwrap_or_else(|| panic!("no session at {}", path.display()))
    }
}

struct Family {
    root: PathBuf,
    p: PathBuf,
    c: PathBuf,
    g: PathBuf,
    f: PathBuf,
    p2: PathBuf,
}

fn family(base: &Path) -> Family {
    let root = base.join("sessions");
    let user = |id: &str, text: &str| fx::user_message(id, None, text);
    let p = fx::write_session(
        &root,
        "b",
        "t1_p",
        &[fx::header("p", None, None), user("e1", "hello")],
    );
    let c = fx::write_session(
        &root,
        "b",
        "t1_p/c",
        &[fx::header("c", None, None), user("c1", "child")],
    );
    let g = fx::write_session(
        &root,
        "b",
        "t1_p/c/g",
        &[fx::header("g", None, None), user("g1", "grandchild")],
    );
    let f = fx::write_session(
        &root,
        "b",
        "t2_f",
        &[fx::header("f", Some("p"), None), user("f1", "fork")],
    );
    let p2 = fx::write_session(
        &root,
        "b",
        "t3_p2",
        &[fx::header("p2", Some("p"), Some("p")), user("n1", "next")],
    );
    Family {
        root,
        p,
        c,
        g,
        f,
        p2,
    }
}

// ---------------------------------------------------------------- relationships

#[test]
fn native_family_links_nesting_fork_and_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let found = discover(&fam.root);

    let parent = found.path(&fam.p).clone();
    let child = found.path(&fam.c).clone();
    let nested = found.path(&fam.g).clone();
    let fork = found.path(&fam.f).clone();
    let continued = found.path(&fam.p2).clone();

    assert_eq!(
        child.lineage.parent,
        Some(SessionLink::Resolved(parent.key.clone()))
    );
    assert!(child.lineage.sidechain);
    assert_eq!(child.lineage.agent_id.as_deref(), Some("c"));
    assert_eq!(
        nested.lineage.parent,
        Some(SessionLink::Resolved(child.key.clone()))
    );
    assert!(nested.lineage.sidechain);

    assert_eq!(
        fork.lineage.fork,
        Some(SessionLink::Resolved(parent.key.clone()))
    );
    assert_eq!(fork.lineage.parent, None);
    assert_ne!(fork.key, parent.key);
    assert!(fork.members.is_empty());

    // The earliest segment is the stable logical root listing the chain in order.
    assert_eq!(
        parent.members,
        vec![parent.key.clone(), continued.key.clone()]
    );
    assert!(continued.members.is_empty());
    assert_eq!(continued.lineage.parent, None);
    assert_eq!(continued.lineage.fork, None);
    assert!(!parent.lineage.sidechain);

    eprintln!("-- synthetic OMP metadata probe --");
    for key in &found.order {
        let m = &found.latest[key];
        eprintln!(
            "id={:<3} parent={:?} fork={:?} sidechain={} members={} activity={:?}/{:?} title={:?} available={}",
            m.key.native_id,
            m.lineage.parent.as_ref().map(link_label),
            m.lineage.fork.as_ref().map(link_label),
            m.lineage.sidechain,
            m.members.len(),
            m.activity.at,
            m.activity.class,
            m.title,
            m.source_available,
        );
    }
}

fn link_label(link: &SessionLink) -> String {
    match link {
        SessionLink::Resolved(key) => format!("Resolved({})", key.native_id),
        SessionLink::Ambiguous => "Ambiguous".into(),
        SessionLink::Unresolved(text) => format!("Unresolved({text})"),
    }
}

#[test]
fn continuation_chain_grows_in_order_and_keeps_its_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let p = fx::write_session(&root, "b", "t1_p", &[fx::header("p", None, None)]);
    let mut catalog = OmpCatalog::new(vec![root.clone()], 100, 1 << 24);
    let mut found = Found::default();
    pass(&mut catalog, BIG, &mut found);
    let root_key = found.path(&p).key.clone();
    assert!(found.path(&p).members.is_empty());

    let p2 = fx::write_session(
        &root,
        "b",
        "t2_p2",
        &[fx::header("p2", Some("p"), Some("p"))],
    );
    pass(&mut catalog, BIG, &mut found);
    let p2_key = found.path(&p2).key.clone();
    assert_eq!(found.path(&p).key, root_key, "root key is stable");
    assert_eq!(
        found.path(&p).members,
        vec![root_key.clone(), p2_key.clone()]
    );

    let p3 = fx::write_session(
        &root,
        "b",
        "t3_p3",
        &[fx::header("p3", Some("p2"), Some("p2"))],
    );
    pass(&mut catalog, BIG, &mut found);
    let p3_key = found.path(&p3).key.clone();
    assert_eq!(found.path(&p).key, root_key);
    assert_eq!(
        found.path(&p).members,
        vec![root_key, p2_key.clone(), p3_key]
    );
    assert!(found.path(&p2).members.is_empty());
}

#[test]
fn missing_conflicting_and_duplicate_evidence_stays_visible_and_unmerged() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let root = &fam.root;
    let missing = fx::write_session(root, "b", "t4_m1", &[fx::header("m1", Some("nope"), None)]);
    let d1 = fx::write_session(root, "b", "t5_d1", &[fx::header("dup", None, None)]);
    let d2 = fx::write_session(root, "b", "t6_d2", &[fx::header("dup", None, None)]);
    let to_dup = fx::write_session(root, "b", "t7_x", &[fx::header("x", Some("dup"), None)]);
    let cont_dup = fx::write_session(
        root,
        "b",
        "t8_y",
        &[fx::header("y", Some("dup"), Some("dup"))],
    );
    // A child whose header names a different discovered session than its directory.
    let conflict = fx::write_session(root, "b", "t1_p/k", &[fx::header("k", Some("f"), None)]);
    let ghost = fx::write_session(
        root,
        "b",
        "t9_z",
        &[fx::header("z", Some("ghost"), Some("ghost"))],
    );
    // Continues "p" but from another directory: not the producer's sibling write.
    let far = fx::write_session(
        &root.clone(),
        "other",
        "t1_far",
        &[fx::header("far", Some("p"), Some("p"))],
    );
    let found = discover(root);

    assert_eq!(
        found.path(&missing).lineage.fork,
        Some(SessionLink::Unresolved("nope".into()))
    );
    // Same header ID, distinct physical identities: separate rows, no merge.
    let (a, b) = (found.path(&d1), found.path(&d2));
    assert_eq!(a.key.native_id, b.key.native_id);
    assert_ne!(a.key, b.key);
    assert!(a.members.is_empty() && b.members.is_empty());
    assert_eq!(
        found.path(&to_dup).lineage.fork,
        Some(SessionLink::Ambiguous)
    );
    let y = found.path(&cont_dup);
    assert_eq!(y.lineage.parent, Some(SessionLink::Ambiguous));
    assert!(y.members.is_empty());

    assert_eq!(
        found.path(&conflict).lineage.parent,
        Some(SessionLink::Ambiguous),
        "header and directory disagree"
    );
    assert_eq!(
        found.path(&ghost).lineage.parent,
        Some(SessionLink::Unresolved(format!("{REJ}ghost")))
    );
    assert_eq!(
        found.path(&far).lineage.parent,
        Some(SessionLink::Unresolved(format!("{REJ}p")))
    );
    // The family still resolves around the damaged siblings.
    assert_eq!(
        found.path(&fam.p).members.len(),
        2,
        "valid continuation unaffected"
    );
}

#[test]
fn two_continuers_and_cycles_are_ambiguous_not_merged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let q = fx::write_session(&root, "b", "t1_q", &[fx::header("q", None, None)]);
    let a = fx::write_session(&root, "b", "t2_a", &[fx::header("a", Some("q"), Some("q"))]);
    let b = fx::write_session(&root, "b", "t3_b", &[fx::header("b", Some("q"), Some("q"))]);
    let x = fx::write_session(&root, "b", "t4_x", &[fx::header("x", Some("y"), Some("y"))]);
    let y = fx::write_session(&root, "b", "t5_y", &[fx::header("y", Some("x"), Some("x"))]);
    let found = discover(&root);
    assert!(found.path(&q).members.is_empty());
    for path in [&a, &b, &x, &y] {
        let meta = found.path(path);
        assert_eq!(meta.lineage.parent, Some(SessionLink::Ambiguous));
        assert!(meta.members.is_empty());
    }
}

#[test]
fn parent_session_path_is_matched_as_text_and_never_opened() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    // A real, large file outside the root with its own header: never read.
    let mut decoy = fx::title_slot("decoy");
    decoy.extend(fx::header("decoy", None, None).to_string().into_bytes());
    decoy.push(b'\n');
    decoy.resize(decoy.len() + (2 << 20), b'x');
    let decoy_path = outside.join("decoy.jsonl");
    fs::write(&decoy_path, &decoy).unwrap();

    let spelled = fam.p.to_string_lossy().into_owned();
    let by_path = fx::write_session(
        &fam.root,
        "b",
        "t4_bypath",
        &[fx::header("bypath", Some(&spelled), None)],
    );
    // The same session file spelled through an alias of the root.
    let alias = format!(
        "/alias/sessions/b/{}",
        fam.p.file_name().unwrap().to_string_lossy()
    );
    let by_alias = fx::write_session(
        &fam.root,
        "b",
        "t5_byalias",
        &[fx::header("byalias", Some(&alias), None)],
    );
    let embedded = fx::write_session(
        &fam.root,
        "b",
        "t6_outside",
        &[fx::header(
            "outside",
            Some(&decoy_path.to_string_lossy()),
            None,
        )],
    );
    perf::reset();
    let found = discover(&fam.root);
    let read = perf::snapshot().bytes_read;
    let total: u64 = tree_size(&fam.root);
    assert!(
        read <= total + 8192,
        "read {read} of {total}: outside file touched"
    );

    let parent_key = found.path(&fam.p).key.clone();
    assert_eq!(
        found.path(&by_path).lineage.fork,
        Some(SessionLink::Resolved(parent_key.clone()))
    );
    assert_eq!(
        found.path(&by_alias).lineage.fork,
        Some(SessionLink::Resolved(parent_key))
    );
    match &found.path(&embedded).lineage.fork {
        Some(SessionLink::Unresolved(text)) => assert!(text.ends_with("decoy.jsonl")),
        other => panic!("outside path must stay unresolved, got {other:?}"),
    }
    assert!(
        !found.latest.values().any(|m| m.key.native_id == "decoy"),
        "the outside file is not a session of this store"
    );
}

#[test]
fn title_and_message_text_cannot_spoof_headers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let spoof = json!({"type":"session","id":"evil","parentSession":"victim"}).to_string();
    let slot = fx::title_slot(&spoof);
    let hostile_line = fx::header("evil2", Some("victim"), Some("victim"));
    let path = fx::write_slotted(
        &root,
        "b",
        "t1_real",
        &slot,
        &[
            fx::header("real", None, None),
            fx::user_message("m1", None, &spoof),
            hostile_line,
            fx::user_message("m2", Some("m1"), "tail"),
        ],
    );
    let victim = fx::write_session(&root, "b", "t0_victim", &[fx::header("victim", None, None)]);
    let found = discover(&root);
    let meta = found.path(&path);
    assert_eq!(meta.key.native_id, "real");
    assert_eq!(meta.lineage.parent, None);
    assert_eq!(meta.lineage.fork, None);
    assert!(meta.members.is_empty());
    assert!(found.path(&victim).members.is_empty());
    assert_eq!(found.latest.len(), 2);
}

#[test]
fn resolve_omp_links_matches_catalog_for_aligned_headers() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let found = discover(&fam.root);
    let mut metas: Vec<SessionMetadata> = found
        .order
        .iter()
        .map(|k| found.latest[k].clone())
        .collect();
    let headers: Vec<OmpHeader> = metas
        .iter()
        .map(|m| {
            let SourceKey::File(path) = &m.key.source else {
                unreachable!()
            };
            let text = fs::read_to_string(path).unwrap();
            let line = text.lines().nth(1).unwrap();
            parse_header(&serde_json::from_str(line).unwrap()).unwrap()
        })
        .collect();
    let expected = metas.clone();
    for m in &mut metas {
        m.lineage = Default::default();
        m.members.clear();
    }
    resolve_omp_links(&mut metas, &headers);
    assert_eq!(metas, expected);
    // Idempotent.
    resolve_omp_links(&mut metas, &headers);
    assert_eq!(metas, expected);
    // Headers that do not belong to their slot are ignored, not misattributed.
    let mut swapped = headers.clone();
    swapped.reverse();
    let mut again = expected.clone();
    resolve_omp_links(&mut again, &swapped);
    let child = again.iter().find(|m| m.key.native_id == "f").unwrap();
    assert_eq!(child.lineage.fork, None);
}

// ---------------------------------------------------------------- probe & slot

#[test]
fn multi_mib_sources_are_read_by_prefix_and_tail_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut body = fx::title_slot("big");
    body.extend(fx::header("big", None, None).to_string().into_bytes());
    body.push(b'\n');
    let filler = fx::user_message("fill", None, &"x".repeat(1000)).to_string();
    for _ in 0..3500 {
        body.extend(filler.as_bytes());
        body.push(b'\n');
    }
    // The middle holds a usage record and a model that discovery must not parse.
    let middle = fx::model_usage("mid", None, "middle", 777).to_string();
    let at = body.len() / 2;
    let tail_start = body[at..].iter().position(|b| *b == b'\n').unwrap() + at + 1;
    let mut spliced = body[..tail_start].to_vec();
    spliced.extend(middle.as_bytes());
    spliced.push(b'\n');
    spliced.extend(&body[tail_start..]);
    let big = fx::write_raw(&root, "b", "t1_big", &spliced);
    assert!(fs::metadata(&big).unwrap().len() > 3 << 20);

    perf::reset();
    let found = discover(&root);
    let used = perf::snapshot();
    assert!(
        used.bytes_read <= 4096 + 32 * 1024 + 8192,
        "read {}",
        used.bytes_read
    );
    let meta = found.path(&big);
    assert_eq!(meta.key.native_id, "big");
    assert!(meta.model.is_none(), "middle usage stays unhydrated");
    assert_eq!(meta.activity.class, AggregationClass::Reported);
    assert_eq!(meta.activity.at, Some(fx::STAMP_MS));
}

#[test]
fn header_only_source_marks_the_fallback_mtime_approximate() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let path = fx::write_session(&root, "b", "t1_quiet", &[fx::header("quiet", None, None)]);
    set_modified(&path, 1_700_000_000);
    let found = discover(&root);
    let meta = found.path(&path);
    assert_eq!(meta.activity.at, Some(1_700_000_000_000));
    assert_eq!(meta.activity.class, AggregationClass::Derived);
    assert_eq!(meta.created_at, Some(fx::STAMP_MS));
    assert_eq!(meta.title, "t1_quiet");
    assert!(!meta.title_derived);
}

#[test]
fn title_slot_counts_bytes_and_rejects_malformed_slots() {
    let slot = fx::title_slot("Fix the é widget");
    assert_eq!(slot.len(), 256);
    assert_eq!(parse_title_slot(&slot).as_deref(), Some("Fix the é widget"));
    // With and without the trailing newline.
    assert_eq!(
        parse_title_slot(&slot[..255]).as_deref(),
        Some("Fix the é widget")
    );

    // Multi-byte title truncated to fit by bytes, still exactly 256.
    let long = fx::title_slot(&"é".repeat(300));
    assert_eq!(long.len(), 256);
    assert!(parse_title_slot(&long).is_some());

    // 256 *characters* but more than 256 bytes is not a slot.
    let object = |title: &str, pad: &str| {
        format!(
            "{}",
            json!({"type":"title","v":1,"title":title,"updatedAt":fx::STAMP,"pad":pad})
        )
    };
    let title = "é".repeat(60);
    let chars = object(&title, "").chars().count() + 1;
    let pad = " ".repeat(256 - chars);
    let by_chars = format!("{}\n", object(&title, &pad));
    assert_eq!(by_chars.chars().count(), 256);
    assert!(by_chars.len() > 256);
    assert_eq!(parse_title_slot(by_chars.as_bytes()), None);

    // Too short, too long, wrong version/type, unknown source, junk, bad UTF-8.
    let short = format!("{}\n", object("t", "")).into_bytes();
    assert_eq!(parse_title_slot(&short), None);
    let mut long_slot = slot.clone();
    long_slot.insert(200, b' ');
    assert_eq!(parse_title_slot(&long_slot), None);
    let text = String::from_utf8(slot.clone()).unwrap();
    assert_eq!(
        parse_title_slot(text.replace("\"v\":1", "\"v\":2").as_bytes()),
        None
    );
    assert_eq!(
        parse_title_slot(text.replace("\"title\"", "\"titel\"").as_bytes()),
        None
    );
    let inner =
        json!({"type":"title","v":1,"title":"t","source":"bogus","updatedAt":fx::STAMP,"pad":""})
            .to_string();
    let pad = " ".repeat(255 - inner.len());
    let bogus =
        json!({"type":"title","v":1,"title":"t","source":"bogus","updatedAt":fx::STAMP,"pad":pad})
            .to_string();
    assert_eq!(bogus.len(), 255);
    assert_eq!(parse_title_slot(bogus.as_bytes()), None);
    assert_eq!(parse_title_slot(&[0xff; 256]), None);
    assert_eq!(parse_title_slot(b""), None);
    assert_eq!(parse_title_slot(b"{}\n"), None);

    // The user/auto source is kept; control characters never reach a title.
    let sourced = json!({"type":"title","v":1,"title":"a\u{7}b\nc","source":"auto","updatedAt":fx::STAMP,"pad":""}).to_string();
    let pad = " ".repeat(255 - sourced.len());
    let sourced = json!({"type":"title","v":1,"title":"a\u{7}b\nc","source":"auto","updatedAt":fx::STAMP,"pad":pad}).to_string();
    assert_eq!(sourced.len(), 255);
    let parsed = parse_title_slot_entry(sourced.as_bytes()).unwrap();
    assert_eq!(parsed.title, "abc");
    assert_eq!(parsed.derived, Some(true));
}

#[test]
fn a_malformed_slot_cannot_swallow_the_header_and_a_good_slot_titles_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let header = fx::header("sid", None, None);

    // A 257-byte slot (one byte too many) followed by the real header.
    let mut oversized = fx::title_slot("never shown");
    oversized.insert(oversized.len() - 1, b' ');
    assert_eq!(oversized.len(), 257);
    let a = fx::write_slotted(
        &root,
        "b",
        "t1_a",
        &oversized,
        std::slice::from_ref(&header),
    );
    // Garbage in place of the slot.
    let b = fx::write_slotted(
        &root,
        "b",
        "t2_b",
        b"{\"type\":\"title\",\"v\":1,\"title\":\n",
        &[fx::header("sidb", None, None)],
    );
    // A legacy file whose first line is the header.
    let legacy = fx::write_raw(
        &root,
        "b",
        "t3_legacy",
        format!("{}\n", fx::header("sidc", None, None)).as_bytes(),
    );
    // No header anywhere: file-name identity with a visible warning.
    let none = fx::write_raw(&root, "b", "t4_noheader", b"{\"hello\":1}\nnot json\n");
    // A valid slot titles the session.
    let good = fx::write_slotted(
        &root,
        "b",
        "t5_good",
        &fx::title_slot("Refactor parser"),
        &[fx::header("sidd", None, None)],
    );
    let found = discover(&root);
    assert_eq!(found.path(&a).key.native_id, "sid");
    assert_eq!(found.path(&a).title, "sid", "malformed slot gives no title");
    assert_eq!(found.path(&b).key.native_id, "sidb");
    assert_eq!(found.path(&legacy).key.native_id, "sidc");
    let n = found.path(&none);
    assert_eq!(n.key.native_id, "noheader");
    assert!(found.has_warning(WarningKind::Parse, "t4_noheader"));
    let g = found.path(&good);
    assert_eq!(g.key.native_id, "sidd");
    assert_eq!(g.title, "Refactor parser");
    assert_eq!(g.cwd.as_deref(), Some(Path::new("/work/demo")));
    assert_eq!(g.project.as_deref(), Some("demo"));
}

#[test]
fn header_parsing_accepts_only_structural_session_headers() {
    let header = parse_header(&fx::header("s1", Some("p"), Some("p"))).unwrap();
    assert_eq!(header.id, "s1");
    assert_eq!(header.parent_session.as_deref(), Some("p"));
    assert_eq!(header.continuation_of.as_deref(), Some("p"));
    assert_eq!(header.created_at, Some(fx::STAMP_MS));
    assert_eq!(header.cwd.as_deref(), Some("/work/demo"));

    assert!(parse_header(&json!({"type":"message","id":"x"})).is_none());
    assert!(parse_header(&json!({"type":"session"})).is_none());
    assert!(parse_header(&json!({"type":"session","id":""})).is_none());
    assert!(parse_header(&json!({"type":"session","id":"a\u{1b}b"})).is_none());
    assert!(parse_header(&json!({"type":"session","id":7})).is_none());
    assert!(parse_header(&json!([1, 2])).is_none());
    assert!(parse_header(&json!({"id":"x"})).is_none());

    let sloppy = parse_header(&json!({
        "type":"session","id":"ok","timestamp":"yesterday","cwd":3,
        "parentSession":["p"],"continuationOf":{"id":"p"}
    }))
    .unwrap();
    assert_eq!(sloppy.created_at, None);
    assert_eq!(sloppy.cwd, None);
    assert_eq!(sloppy.parent_session, None);
    assert_eq!(sloppy.continuation_of, None);

    let debug = format!("{header:?}");
    assert!(
        !debug.contains("s1") && !debug.contains("/work/demo"),
        "{debug}"
    );
}

// ---------------------------------------------------------------- catalog

#[test]
fn unsupported_and_unrelated_files_warn_without_hiding_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let bucket = fam.root.join("b");
    fs::write(bucket.join("t9_old.jsonl.gz"), b"\x1f\x8b junk").unwrap();
    fs::write(bucket.join("t9_old.jsonl.bak"), b"{}\n").unwrap();
    fs::write(bucket.join("notes.txt"), b"x").unwrap();
    // Blob/tool-output directories are never entered, even holding session-like files.
    let blobs = bucket.join("blobs");
    fs::create_dir_all(&blobs).unwrap();
    fs::write(
        blobs.join("trap.jsonl"),
        format!("{}\n", fx::header("trap", None, None)),
    )
    .unwrap();
    let artifacts = bucket.join("t1_p");
    fs::write(artifacts.join("0.bash.log"), b"output").unwrap();
    // A root-level file is not a bucket.
    fs::write(
        fam.root.join("stray.jsonl"),
        format!("{}\n", fx::header("stray", None, None)),
    )
    .unwrap();

    let found = discover(&fam.root);
    assert!(found.has_warning(WarningKind::Unsupported, "t9_old.jsonl.gz"));
    for id in ["p", "c", "g", "f", "p2"] {
        found.by_id(id);
    }
    assert!(!found.latest.values().any(|m| m.key.native_id == "trap"));
    assert!(!found.latest.values().any(|m| m.key.native_id == "stray"));
    assert_eq!(found.latest.len(), 5);
}

#[test]
fn tiny_budgets_make_progress_and_match_one_big_pass() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let whole = discover(&fam.root);

    let tiny = RefreshBudget {
        max_records: 1,
        max_bytes: 1,
        max_millis: 0,
    };
    let mut catalog = OmpCatalog::new(vec![fam.root.clone()], 1000, 64 << 20);
    let mut found = Found::default();
    pass(&mut catalog, tiny, &mut found);
    assert!(found.slices >= 5, "slices {}", found.slices);
    assert_eq!(found.latest, whole.latest);

    // An unchanged second pass reports nothing new.
    let mut again = Found::default();
    pass(&mut catalog, tiny, &mut again);
    assert_eq!(again.reported, 0);
    assert!(again.removed.is_empty());
}

#[test]
fn replacement_shrink_and_removal_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let long_text = "y".repeat(2000);
    let path = fx::write_session(
        &root,
        "b",
        "t1_s",
        &[
            fx::header("s", None, None),
            fx::user_message("a", None, &long_text),
        ],
    );
    let mut catalog = OmpCatalog::new(vec![root.clone()], 100, 1 << 24);
    let mut found = Found::default();
    pass(&mut catalog, BIG, &mut found);
    let old_key = found.path(&path).key.clone();

    // Same identity, smaller file: the readable content was rewritten.
    fx::write_session(&root, "b", "t1_s", &[fx::header("s", None, None)]);
    let mut shrunk = Found::default();
    pass(&mut catalog, BIG, &mut shrunk);
    assert_eq!(shrunk.invalidated, vec![old_key.clone()]);
    assert!(shrunk.removed.is_empty());

    // Another session replaces the file: the old identity is removed.
    fx::write_session(
        &root,
        "b",
        "t1_s",
        &[
            fx::header("replacement", None, None),
            fx::user_message("a", None, &long_text),
        ],
    );
    let mut replaced = Found::default();
    pass(&mut catalog, BIG, &mut replaced);
    assert_eq!(replaced.removed, vec![old_key]);
    assert_eq!(replaced.path(&path).key.native_id, "replacement");

    // Deleted: removed with a visible warning.
    let new_key = replaced.path(&path).key.clone();
    fs::remove_file(&path).unwrap();
    let mut gone = Found::default();
    pass(&mut catalog, BIG, &mut gone);
    assert_eq!(gone.removed, vec![new_key]);
    assert!(gone.has_warning(WarningKind::Unavailable, "t1_s"));
}

#[test]
fn source_ceiling_and_depth_limit_are_visible() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    for n in 0..4 {
        fx::write_session(
            &root,
            "b",
            &format!("t{n}_s{n}"),
            &[fx::header(&format!("s{n}"), None, None)],
        );
    }
    let mut catalog = OmpCatalog::new(vec![root.clone()], 2, 1 << 24);
    let mut found = Found::default();
    pass(&mut catalog, BIG, &mut found);
    assert_eq!(found.latest.len(), 2);
    assert!(
        found
            .warnings
            .iter()
            .any(|w| w.key.source == "metadata-limit")
    );

    // Byte ceiling.
    let mut tight = OmpCatalog::new(vec![root.clone()], 100, 1);
    let mut none = Found::default();
    pass(&mut tight, BIG, &mut none);
    assert!(none.latest.is_empty());
    assert!(
        none.warnings
            .iter()
            .any(|w| w.key.source == "metadata-limit")
    );

    // A pathological artifact nesting stops at the depth ceiling with a warning.
    let deep = dir.path().join("deep");
    let mut name = String::from("t0_l0");
    for level in 0..13 {
        fx::write_session(
            &deep,
            "b",
            &name,
            &[fx::header(&format!("l{level}"), None, None)],
        );
        name = format!("{name}/l{}", level + 1);
    }
    let found = discover(&deep);
    assert!(found.has_warning(WarningKind::Unsupported, "t0_l0"));
    assert!(found.latest.len() < 13);
    found.by_id("l0");
    found.by_id("l1");
}

#[test]
fn overlapping_and_missing_roots_are_handled() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let mut catalog = OmpCatalog::new(
        vec![
            fam.root.clone(),
            fam.root.join("b"),
            fam.root.clone(),
            dir.path().join("absent"),
        ],
        1000,
        64 << 20,
    );
    let mut found = Found::default();
    pass(&mut catalog, BIG, &mut found);
    assert_eq!(found.latest.len(), 5);
    assert!(found.has_warning(WarningKind::Unavailable, "absent"));
}

#[test]
fn unsupported_sibling_does_not_hide_valid_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let good = fx::write_session(&root, "b", "t1_good", &[fx::header("good", None, None)]);
    // Binary garbage and a torn final line.
    fx::write_raw(
        &root,
        "b",
        "t2_junk",
        &[0u8, 159, 146, 150, 10, 255, 254, 10],
    );
    let mut torn = fx::title_slot("torn");
    torn.extend(fx::header("torn", None, None).to_string().into_bytes());
    torn.extend(b"\n{\"type\":\"message\",\"id\":\"half");
    let torn_path = fx::write_raw(&root, "b", "t3_torn", &torn);
    let found = discover(&root);
    assert_eq!(found.path(&good).key.native_id, "good");
    assert_eq!(found.path(&torn_path).key.native_id, "torn");
    assert_eq!(found.latest.len(), 3);
    assert!(found.has_warning(WarningKind::Parse, "t2_junk"));
}

// ---------------------------------------------------------------- tree

fn eh(id: &str, parent: Option<&str>, leaf: ActiveLeaf, ordinal: u64) -> OmpEntryHeader {
    OmpEntryHeader {
        id: id.to_owned(),
        parent_id: parent.map(str::to_owned),
        active_leaf: leaf,
        ordinal,
    }
}

fn tree_of(entries: Vec<OmpEntryHeader>) -> OmpTree {
    let mut tree = OmpTree::new(100_000, 64 << 20);
    for entry in entries {
        tree.insert(entry);
    }
    tree
}

#[test]
fn absent_leaf_selects_the_entry_and_null_clears_it() {
    use ActiveLeaf::{Absent, Cleared};
    let absent = tree_of(vec![
        eh("a", None, Absent, 0),
        eh("b", Some("a"), Absent, 1),
        eh("c", Some("b"), Absent, 2),
    ]);
    assert_eq!(absent.active_path(), Ok(vec![0, 1, 2]));

    let cleared = tree_of(vec![
        eh("a", None, Absent, 0),
        eh("b", Some("a"), Absent, 1),
        eh("c", Some("b"), Cleared, 2),
    ]);
    assert_eq!(cleared.active_path(), Ok(vec![]));

    assert_eq!(OmpTree::new(10, 1000).active_path(), Ok(vec![]));
}

#[test]
fn the_three_states_survive_parsing_from_record_shape() {
    let plain = fx::entry("a", None, "message", json!({}));
    let null = fx::with_leaf(fx::entry("b", Some("a"), "message", json!({})), None);
    let set = fx::with_leaf(
        fx::entry("c", Some("a"), "model_usage", json!({})),
        Some("a"),
    );
    assert_eq!(
        parse_entry_header(&plain, 0).unwrap().active_leaf,
        ActiveLeaf::Absent
    );
    assert_eq!(
        parse_entry_header(&null, 1).unwrap().active_leaf,
        ActiveLeaf::Cleared
    );
    assert_eq!(
        parse_entry_header(&set, 2).unwrap().active_leaf,
        ActiveLeaf::Set("a".into())
    );
    // Odd shapes are not indexed rather than guessed; non-entries are not entries.
    assert!(parse_entry_header(&json!({"type":"message","id":"x","activeLeafId":5}), 0).is_none());
    assert!(parse_entry_header(&json!({"type":"message","id":"x","parentId":5}), 0).is_none());
    assert!(parse_entry_header(&json!({"type":"message"}), 0).is_none());
    assert!(parse_entry_header(&json!({"type":"message","id":""}), 0).is_none());
    assert!(parse_entry_header(&fx::header("s", None, None), 0).is_none());
    assert!(parse_entry_header(&json!({"type":"title","v":1,"id":"t","title":"x"}), 0).is_none());

    // Debug never prints ids.
    let debug = format!("{:?}", parse_entry_header(&set, 2).unwrap());
    assert!(
        !debug.contains("\"a\"") && !debug.contains("Set(a"),
        "{debug}"
    );
}

#[test]
fn an_off_branch_write_keeps_the_captured_leaf() {
    use ActiveLeaf::{Absent, Set};
    // a ← b (older branch), a ← c ← late usage written after, naming leaf b.
    let off = tree_of(vec![
        eh("a", None, Absent, 0),
        eh("b", Some("a"), Absent, 1),
        eh("c", Some("a"), Absent, 2),
        eh("late", Some("c"), Set("b".into()), 3),
    ]);
    assert_eq!(off.active_path(), Ok(vec![0, 1]));

    // The same record without the captured leaf selects itself.
    let own = tree_of(vec![
        eh("a", None, Absent, 0),
        eh("b", Some("a"), Absent, 1),
        eh("c", Some("a"), Absent, 2),
        eh("late", Some("c"), Absent, 3),
    ]);
    assert_eq!(own.active_path(), Ok(vec![0, 2, 3]));

    // File order, not ordinal gaps or timestamps, picks the last entry.
    let gaps = tree_of(vec![
        eh("a", None, Absent, 10),
        eh("b", Some("a"), Absent, 500),
        eh("c", Some("a"), Absent, 20),
    ]);
    assert_eq!(gaps.active_path(), Ok(vec![10, 500]));
}

#[test]
fn missing_targets_cycles_and_limits_are_reported_not_repaired() {
    use ActiveLeaf::{Absent, Set};
    let missing_leaf = tree_of(vec![
        eh("a", None, Absent, 0),
        eh("b", Some("a"), Set("gone".into()), 1),
    ]);
    assert_eq!(missing_leaf.active_path(), Err(OmpTreeError::MissingLeaf));

    let missing_parent = tree_of(vec![
        eh("a", Some("ghost"), Absent, 0),
        eh("b", Some("a"), Absent, 1),
    ]);
    assert_eq!(missing_parent.active_path(), Err(OmpTreeError::MissingLeaf));

    let cycle = tree_of(vec![
        eh("a", Some("b"), Absent, 0),
        eh("b", Some("a"), Absent, 1),
    ]);
    assert_eq!(cycle.active_path(), Err(OmpTreeError::Cycle));
    let selfie = tree_of(vec![eh("a", Some("a"), Absent, 0)]);
    assert_eq!(selfie.active_path(), Err(OmpTreeError::Cycle));

    let mut small = OmpTree::new(2, 1 << 20);
    assert_eq!(small.insert(eh("a", None, Absent, 0)), OmpInsert::Inserted);
    assert_eq!(
        small.insert(eh("b", Some("a"), Absent, 1)),
        OmpInsert::Inserted
    );
    assert_eq!(
        small.insert(eh("c", Some("b"), Absent, 2)),
        OmpInsert::Limited
    );
    assert!(small.is_limited());
    assert_eq!(small.len(), 2);
    assert_eq!(small.active_path(), Err(OmpTreeError::Limited));

    let mut bytes = OmpTree::new(1_000, 100);
    assert_eq!(bytes.insert(eh("a", None, Absent, 0)), OmpInsert::Inserted);
    assert_eq!(
        bytes.insert(eh("b", Some("a"), Absent, 1)),
        OmpInsert::Limited
    );
    assert_eq!(bytes.active_path(), Err(OmpTreeError::Limited));
}

#[test]
fn duplicate_ids_keep_first_structure_and_deep_chains_do_not_recurse() {
    use ActiveLeaf::Absent;
    let mut tree = OmpTree::new(100, 1 << 20);
    assert_eq!(tree.insert(eh("a", None, Absent, 0)), OmpInsert::Inserted);
    assert_eq!(
        tree.insert(eh("b", Some("a"), Absent, 1)),
        OmpInsert::Inserted
    );
    // Copied-history repeat of "a" with another parent: first structure wins.
    assert_eq!(
        tree.insert(eh("a", Some("b"), Absent, 2)),
        OmpInsert::Duplicate
    );
    assert_eq!(tree.len(), 2);
    // The repeated line is the newest, so it still selects itself as the leaf.
    assert_eq!(tree.active_path(), Ok(vec![0]));

    let mut deep = OmpTree::new(200_000, 64 << 20);
    let mut parent: Option<String> = None;
    for n in 0..100_000u64 {
        let id = format!("e{n}");
        deep.insert(eh(&id, parent.as_deref(), Absent, n));
        parent = Some(id);
    }
    let path = deep.active_path().unwrap();
    assert_eq!(path.len(), 100_000);
    assert_eq!(path.first(), Some(&0));
    assert_eq!(path.last(), Some(&99_999));
}

#[test]
fn physical_entry_lines_feed_the_tree_in_file_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let entries = vec![
        fx::header("t", None, None),
        fx::user_message("a", None, "one"),
        fx::assistant("b", Some("a"), 100, 2000),
        fx::model_usage("aux", Some("b"), "title", 5),
        fx::compaction("cmp", Some("b"), "b"),
        fx::task_call("task", Some("cmp"), "agent-1"),
        fx::with_leaf(fx::model_usage("late", Some("task"), "title", 5), Some("b")),
        fx::exit("end", Some("late")),
    ];
    let path = fx::write_session(&root, "b", "t1_t", &entries);
    let text = fs::read_to_string(&path).unwrap();
    let mut tree = OmpTree::new(100, 1 << 20);
    for (ordinal, line) in text.lines().enumerate() {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue, // the physical title slot is padded JSON too
        };
        if let Some(header) = parse_entry_header(&value, ordinal as u64) {
            tree.insert(header);
        }
    }
    // The last line `end` selects itself: a ← b ← cmp ← task ← late ← end (the
    // auxiliary model_usage `aux` is a dead-end child, not on the branch).
    assert_eq!(tree.len(), 7);
    assert_eq!(tree.active_path().unwrap().len(), 6);
}

fn tree_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| {
            let path = e.path();
            if path.is_dir() {
                tree_size(&path)
            } else {
                e.metadata().unwrap().len()
            }
        })
        .sum()
}

// ---------------------------------------------------------------- review fixes

#[test]
fn header_past_the_prefix_window_is_read_through_its_newline() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let cwd = format!("/work/{}", "d".repeat(3000));
    let parent = format!("/work/{}.jsonl", "q".repeat(2000));
    let mut header = fx::header("longhdr", Some(&parent), None);
    header["cwd"] = json!(cwd);
    let line = header.to_string();
    // The 256-byte slot plus this header crosses byte 4096.
    assert!(256 + line.len() > 4096);
    let path = fx::write_session(
        &root,
        "b",
        "t1_longhdr",
        &[header, fx::user_message("m", None, "x")],
    );
    let found = discover(&root);
    let meta = found.path(&path);
    assert_eq!(meta.key.native_id, "longhdr", "no file-name fallback");
    assert_eq!(meta.cwd.as_deref(), Some(Path::new(cwd.as_str())));
    assert!(!found.has_warning(WarningKind::Parse, "t1_longhdr"));
    assert!(matches!(
        meta.lineage.fork,
        Some(SessionLink::Unresolved(_))
    ));
}

#[test]
fn rejected_continuation_is_not_reinterpreted_by_the_roster() {
    use agentscope::domain::Harness;
    use agentscope::providers::DiscoveryDelta;
    use agentscope::roster::{MetadataLimits, RosterIndex};
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let far = fx::write_session(
        &fam.root,
        "other",
        "t1_far",
        &[fx::header("far", Some("p"), Some("p"))],
    );
    let found = discover(&fam.root);
    let far_meta = found.path(&far).clone();
    match &far_meta.lineage.parent {
        Some(SessionLink::Unresolved(text)) => {
            assert!(text.starts_with(agentscope::providers::omp::links::REJECTED_CONTINUATION));
            assert!(text.ends_with('p'));
        }
        other => panic!("expected rejected-continuation evidence, got {other:?}"),
    }
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 100,
            max_bytes: 64 << 20,
        },
        16,
    );
    index.apply_discovery(DiscoveryDelta {
        sessions: found.latest.values().cloned().collect(),
        removed: vec![],
        invalidated: vec![],
        warnings: vec![],
        scan: agentscope::domain::ScanProgress {
            harness: Harness::Omp,
            scanned: 6,
            total: Some(6),
            done: true,
        },
    });
    index.freeze(1);
    let snapshot = index.snapshot();
    assert!(snapshot.roots.contains(&far_meta.key), "far stays a root");
    let p = found.path(&fam.p).key.clone();
    assert_eq!(index.child_count(&p), 1, "only the artifact child c");
    assert!(
        !index
            .children(&p, None, 50)
            .metadata
            .iter()
            .any(|m| m.key == far_meta.key)
    );
}

#[test]
fn growth_after_admission_respects_the_byte_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let path = fx::write_session(&root, "b", "t1_g", &[fx::header("g", None, None)]);
    let mut probe = OmpCatalog::new(vec![root.clone()], 100, 1 << 24);
    let mut first = Found::default();
    pass(&mut probe, BIG, &mut first);
    let small = agentscope::roster::metadata_bytes(first.path(&path)) + path.as_os_str().len();

    let mut catalog = OmpCatalog::new(vec![root.clone()], 100, small + 200);
    let mut found = Found::default();
    pass(&mut catalog, BIG, &mut found);
    assert_eq!(found.latest.len(), 1);

    let mut header = fx::header("g", None, None);
    header["cwd"] = json!(format!("/w/{}", "c".repeat(3000)));
    fx::write_session(&root, "b", "t1_g", &[header]);
    let mut grown = Found::default();
    pass(&mut catalog, BIG, &mut grown);
    assert!(
        grown
            .warnings
            .iter()
            .any(|w| w.key.source == "metadata-limit")
    );
    assert_eq!(grown.reported, 0, "the oversized rewrite is not admitted");
    assert!(
        grown.removed.is_empty(),
        "the admitted session is not dropped"
    );
    assert_eq!(
        found.path(&path).cwd.as_deref(),
        Some(Path::new("/work/demo"))
    );
}

#[test]
fn conflicting_continuation_on_an_artifact_child_stays_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let fam = family(dir.path());
    let k = fx::write_session(
        &fam.root,
        "b",
        "t1_p/k",
        &[fx::header("k", Some("p"), Some("zzz"))],
    );
    let found = discover(&fam.root);
    assert_eq!(found.path(&k).lineage.parent, Some(SessionLink::Ambiguous));
    // A plain artifact child is unaffected.
    assert!(matches!(
        found.path(&fam.c).lineage.parent,
        Some(SessionLink::Resolved(_))
    ));
}

#[test]
fn evicted_published_source_still_reports_its_deletion() {
    use agentscope::domain::Harness;
    use agentscope::providers::DiscoveryDelta;
    use agentscope::roster::{MetadataLimits, RosterIndex};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let a = fx::write_session(&root, "b", "t1_a", &[fx::header("a", None, None)]);
    let mut catalog = OmpCatalog::new(vec![root.clone()], 1, 1 << 24);
    let mut found = Found::default();
    pass(&mut catalog, BIG, &mut found);
    let a_key = found.path(&a).key.clone();
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 100,
            max_bytes: 64 << 20,
        },
        16,
    );
    let apply = |index: &mut RosterIndex, sessions, removed| {
        index.apply_discovery(DiscoveryDelta {
            sessions,
            removed,
            invalidated: vec![],
            warnings: vec![],
            scan: agentscope::domain::ScanProgress {
                harness: Harness::Omp,
                scanned: 1,
                total: Some(1),
                done: true,
            },
        })
    };
    apply(&mut index, found.latest.values().cloned().collect(), vec![]);

    // A strictly newer B displaces A from the one-source catalog.
    let mut newer = fx::header("b", None, None);
    newer["timestamp"] = json!("2027-01-01T00:00:00.000Z");
    let b = fx::write_session(
        &root,
        "b",
        "t2_b",
        &[newer, {
            let mut e = fx::user_message("m", None, "x");
            e["timestamp"] = json!("2027-01-01T00:00:00.000Z");
            e
        }],
    );
    let mut second = Found::default();
    pass(&mut catalog, BIG, &mut second);
    assert!(second.latest.values().any(|m| m.key.native_id == "b"));
    assert!(second.removed.is_empty(), "eviction is not deletion");
    let _ = b;

    // Still present: no removal, and discovery stays within the bounded probes.
    let mut idle = Found::default();
    pass(&mut catalog, BIG, &mut idle);
    assert!(idle.removed.is_empty());

    fs::remove_file(&a).unwrap();
    let mut gone = Found::default();
    pass(&mut catalog, BIG, &mut gone);
    assert_eq!(gone.removed, vec![a_key.clone()]);
    assert!(gone.has_warning(WarningKind::Unavailable, "t1_a"));
    apply(&mut index, vec![], gone.removed.clone());
    assert!(!index.contains(&a_key));
    // Reported once only.
    let mut after = Found::default();
    pass(&mut catalog, BIG, &mut after);
    assert!(after.removed.is_empty());
}

#[test]
fn forgotten_tracking_is_bounded_and_never_reads_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut catalog = OmpCatalog::new(vec![root.clone()], 1, 1 << 24);
    let mut found = Found::default();
    for n in 0..200u32 {
        let mut h = fx::header(&format!("s{n}"), None, None);
        h["timestamp"] = json!(format!("2026-10-30T00:{:02}:{:02}.000Z", n / 60, n % 60));
        let mut e = fx::user_message("m", None, "x");
        e["timestamp"] = h["timestamp"].clone();
        fx::write_session(&root, "b", &format!("t{n:03}_s{n}"), &[h, e]);
        pass(&mut catalog, BIG, &mut found);
    }
    assert!(found.removed.is_empty(), "eviction is not deletion");
    perf::reset();
    let mut idle = Found::default();
    pass(&mut catalog, BIG, &mut idle);
    assert!(idle.removed.is_empty());
    assert!(
        perf::snapshot().bytes_read <= 2 * (4096 + 32 * 1024 + 8192) * 200,
        "idle pass reads only bounded probes"
    );
    // Deleting everything reports at most the bounded watch set, by metadata only.
    fs::remove_dir_all(&root).unwrap();
    let mut gone = Found::default();
    pass(&mut catalog, BIG, &mut gone);
    assert!(!gone.removed.is_empty() && gone.removed.len() <= 64 + 1);
}
