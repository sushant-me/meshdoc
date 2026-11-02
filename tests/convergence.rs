//! Convergence under adversarial partition schedules.
//!
//! The claim this file supports: *for arbitrary sequences of edits, partitions and
//! reconnections, all replicas converge to identical state and lose nothing.*
//!
//! The schedule generator is seeded, so any failure is reproducible from its seed.
//! Each scenario ends with a full mesh reconciliation; if two replicas differ after
//! that, the CRDT or the sync layer is wrong.

use std::collections::BTreeSet;

use meshdoc::crypto::Identity;
use meshdoc::docs::DocType;
use meshdoc::ids::{DocId, WorkspaceId};
use meshdoc::node::{Invite, Node};

/// Small deterministic PRNG (xorshift64*) so failures are reproducible and the
/// test needs no dependencies.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next_u64() % 100 < pct
    }
}

struct Cluster {
    nodes: Vec<Node>,
}

impl Cluster {
    /// Build a single workspace with `n` members.
    fn new(n: usize) -> Self {
        let founder = Node::create_workspace(Identity::generate(), "Cluster").unwrap();
        let mut nodes = vec![founder];
        for _ in 1..n {
            let joiner = Identity::generate();
            let invite = nodes[0].invite_for(&joiner.public()).unwrap();
            let node = Node::join_from_invite(joiner, &invite).unwrap();
            nodes.push(node);
        }
        Self { nodes }
    }

    /// Reconcile two replicas in both directions (one "contact").
    fn contact(&mut self, i: usize, j: usize) {
        assert_ne!(i, j);
        let (a, b) = if i < j { (i, j) } else { (j, i) };
        let (left, right) = self.nodes.split_at_mut(b);
        let _ = left[a].sync_with(&mut right[0]);
    }

    /// Full mesh reconciliation: everyone talks to everyone, twice, so every
    /// change has a path to every replica.
    fn heal(&mut self) {
        for _ in 0..2 {
            for i in 0..self.nodes.len() {
                for j in 0..self.nodes.len() {
                    if i != j {
                        self.contact(i, j);
                    }
                }
            }
        }
    }

    fn renders(&self) -> Vec<String> {
        self.nodes.iter().map(|n| n.render()).collect()
    }

    fn assert_converged(&self, seed: u64, step: usize) {
        // Same op set is not enough: every replica must also have applied them all.
        for (i, n) in self.nodes.iter().enumerate() {
            assert_eq!(
                n.applied_count(),
                n.change_count(),
                "seed={seed} step={step} replica {i} holds {} changes but applied {}",
                n.change_count(),
                n.applied_count()
            );
        }
        // Nothing may be held-but-unapplied after a full heal.
        for (i, n) in self.nodes.iter().enumerate() {
            if n.rejected_count() > 0 {
                let mut msg = format!(
                    "seed={seed} step={step} replica {i} rejected {} change(s):",
                    n.rejected_count()
                );
                for (id, why) in n.rejected() {
                    msg.push_str(&format!("\n  {} -> {why}", &id.to_hex()[..10]));
                }
                panic!("{msg}");
            }
        }
        let r = self.renders();
        for w in r.windows(2) {
            assert_eq!(
                w[0], w[1],
                "DIVERGENCE at seed={seed} step={step}\n--- replica A ---\n{}\n--- replica B ---\n{}",
                w[0], w[1]
            );
        }
    }

    fn total_changes(&self) -> usize {
        self.nodes[0].change_count()
    }
}

fn workspace_with_doc(nodes: &mut [Node], ty: DocType, title: &str) -> DocId {
    let did = nodes[0].create_doc(ty, title).unwrap();
    // Replicate the document's existence to everyone before the schedule starts.
    let n = nodes.len();
    for _ in 0..2 {
        for i in 0..n {
            for j in 0..n {
                if i != j {
                    let (a, b) = if i < j { (i, j) } else { (j, i) };
                    let (l, r) = nodes.split_at_mut(b);
                    let _ = l[a].sync_with(&mut r[0]);
                }
            }
        }
    }
    did
}

/// Random edits interleaved with random contacts, then a full heal.
/// This is the workhorse: many seeds, several document types, varying depth.
#[test]
fn random_schedules_always_converge() {
    let seeds: u64 = std::env::var("SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let start: u64 = std::env::var("START")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut worst = 0usize;
    for seed in start..(start + seeds) {
        let mut rng = Rng::new(seed + 1);
        let n = 3 + rng.below(3); // 3..=5 replicas
        let mut c = Cluster::new(n);

        let checklist = workspace_with_doc(&mut c.nodes, DocType::Checklist, "Casualties");
        let form = workspace_with_doc(&mut c.nodes, DocType::SupplyForm, "Needs");
        let note = workspace_with_doc(&mut c.nodes, DocType::Note, "Sitrep");
        let geo = workspace_with_doc(&mut c.nodes, DocType::GeoPoint, "Staging");
        let docs = [checklist, form, note, geo];

        let steps = 30 + rng.below(40);
        for step in 0..steps {
            match rng.below(10) {
                0..=4 => {
                    // Local edit on a random replica.
                    let who = rng.below(n);
                    let doc = docs[rng.below(docs.len())];
                    let node = &mut c.nodes[who];
                    if node.doc(&doc).is_none() {
                        continue;
                    }
                    match rng.below(5) {
                        0 => {
                            node.add_checklist_item(doc, &format!("item-{seed}-{step}"));
                        }
                        1 => {
                            let items = node.doc(&doc).map(|d| d.iter_items()).unwrap_or_default();
                            if !items.is_empty() {
                                let it = &items[rng.below(items.len())];
                                let _ = node.toggle_checklist_item(doc, &it.id, !it.done);
                            }
                        }
                        2 => {
                            node.set_supply_field(
                                doc,
                                &format!("f{}", rng.below(3)),
                                &format!("{}", rng.below(100)),
                                "kg",
                            );
                        }
                        3 => {
                            let _ = node.set_note(doc, &format!("revision {step} seed {seed}"));
                        }
                        _ => {
                            let _ = node.set_location(
                                doc,
                                27.0 + step as f64 * 0.001,
                                85.0,
                                &format!("p{step}"),
                                "water",
                            );
                        }
                    }
                }
                _ => {
                    // A contact between two random replicas.
                    let i = rng.below(n);
                    let j = rng.below(n);
                    if i != j {
                        c.contact(i, j);
                    }
                }
            }
        }

        c.heal();
        c.assert_converged(seed, steps);

        // Nothing was lost: every replica holds the same change count.
        let expected = c.total_changes();
        for (idx, node) in c.nodes.iter().enumerate() {
            assert_eq!(
                node.change_count(),
                expected,
                "seed={seed}: replica {idx} has {} changes, expected {expected}",
                node.change_count()
            );
        }
        worst = worst.max(expected);
    }
    assert!(worst > 0, "schedules should have produced changes");
}

/// Adversarial shape: every replica edits while fully partitioned, then heals.
#[test]
fn full_partition_then_heal_converges() {
    for seed in 0..25u64 {
        let mut rng = Rng::new(seed + 500);
        let n = 4;
        let mut c = Cluster::new(n);
        let did = workspace_with_doc(&mut c.nodes, DocType::Checklist, "Partitioned");

        // Everyone edits alone; no contacts at all.
        for i in 0..n {
            for k in 0..5 {
                let _ = c.nodes[i].add_checklist_item(did, &format!("node{i}-item{k}"));
            }
        }
        // Heal and require convergence.
        c.heal();
        c.assert_converged(seed, 0);
        let total: usize = c
            .nodes
            .iter()
            .map(|x| x.doc(&did).unwrap().iter_items().len())
            .sum();
        assert_eq!(
            total,
            n * 5 * n,
            "each replica must end with all {n}x5 items (seed {seed})"
        );
        let _ = rng.below(1);
    }
}

/// Sequential contacts must propagate everything (epidemic / gossip semantics).
#[test]
fn gossip_chain_propagates_end_to_end() {
    let mut c = Cluster::new(5);
    let did = workspace_with_doc(&mut c.nodes, DocType::Note, "Chain");
    c.nodes[0].set_note(did, "origin").unwrap();

    // A -> B -> C -> D -> E, never a direct contact with the origin.
    for i in 0..4 {
        c.contact(i, i + 1);
    }
    c.heal();

    for node in &c.nodes {
        assert_eq!(node.doc(&did).unwrap().text(), "origin");
    }
    c.assert_converged(0, 0);
}

/// Repeated sync when already converged must be a no-op (no change explosion).
#[test]
fn repeated_sync_is_idempotent() {
    let mut c = Cluster::new(3);
    let did = workspace_with_doc(&mut c.nodes, DocType::Checklist, "Idem");
    c.nodes[0].add_checklist_item(did, "a").unwrap();
    c.heal();
    let before = c.nodes[0].change_count();
    for _ in 0..5 {
        c.heal();
    }
    assert_eq!(
        c.nodes[0].change_count(),
        before,
        "idempotent heal must not create changes"
    );
    c.assert_converged(0, 0);
}

/// A 6-replica, 200-step schedule: the long run.
#[test]
fn long_schedule_six_replicas() {
    let mut rng = Rng::new(0xDEAD_BEEF);
    let n = 6;
    let mut c = Cluster::new(n);
    let did = workspace_with_doc(&mut c.nodes, DocType::Checklist, "Long");

    for step in 0..200 {
        if rng.chance(60) {
            let who = rng.below(n);
            c.nodes[who]
                .add_checklist_item(did, &format!("s{step}"))
                .unwrap();
        } else {
            let i = rng.below(n);
            let j = rng.below(n);
            if i != j {
                c.contact(i, j);
            }
        }
    }
    c.heal();
    c.assert_converged(0xBEEF, 200);

    // The union of all edits must be present everywhere.
    let items = c.nodes[0].doc(&did).unwrap().iter_items();
    let uniq: BTreeSet<String> = items.iter().map(|i| i.text.clone()).collect();
    assert_eq!(
        uniq.len(),
        items.len(),
        "no duplicate items after convergence"
    );
    assert!(
        items.len() >= 200 * 60 / 100,
        "expected at least the number of edit steps"
    );
}

/// Two workspaces in the same store must never leak into each other.
#[test]
fn workspaces_are_isolated() {
    let a = Node::create_workspace(Identity::generate(), "A").unwrap();
    let b = Node::create_workspace(Identity::generate(), "B").unwrap();
    let mut a = a;
    let mut b = b;
    let da = a.create_doc(DocType::Note, "in-a").unwrap();
    let db = b.create_doc(DocType::Note, "in-b").unwrap();
    a.set_note(da, "secret-a").unwrap();
    b.set_note(db, "secret-b").unwrap();

    let res = a.sync_with(&mut b);
    assert!(res.is_err(), "syncing across workspaces must be refused");
    assert!(a.doc(&db).is_none());
    assert!(b.doc(&da).is_none());
}

/// The workspace id must never change, even after membership changes.
#[test]
fn workspace_id_is_stable_across_membership_changes() {
    let mut founder = Node::create_workspace(Identity::generate(), "Stable").unwrap();
    let ws: WorkspaceId = founder.workspace_id;
    for _ in 0..5 {
        let j = Identity::generate();
        let inv = founder.invite_for(&j.public()).unwrap();
        let mut joiner = Node::join_from_invite(j, &inv).unwrap();
        assert_eq!(
            joiner.workspace_id, ws,
            "invitee must land in the same workspace"
        );
        let _ = joiner.sync_with(&mut founder);
    }
    assert_eq!(founder.workspace_id, ws);
}
