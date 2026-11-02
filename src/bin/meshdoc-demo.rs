//! `meshdoc-demo`, an end-to-end demonstration of the core loop.
//!
//! Runs a scripted scenario in one process and prints what each device sees, so
//! the behaviour can be checked without radios or a second machine:
//!
//! 1. Alice creates a workspace; Bob joins via an invite token.
//! 2. Both edit while disconnected (a partition).
//! 3. They meet: anti-entropy reconciles and both converge.
//! 4. Carol joins and receives everything through gossip, she never meets Alice.
//! 5. An outsider forges a change; every honest replica refuses it.
//! 6. Alice pushes to the relay and goes offline; HQ pulls later (DTN).
//!
//! Exit code 0 means every assertion in the scenario held.

use std::time::Instant;

use meshdoc::change::ChangeEnvelope;
use meshdoc::crypto::Identity;
use meshdoc::docs::DocType;
use meshdoc::ids::Epoch;
use meshdoc::node::{Invite, Node};

fn hr(title: &str) {
    println!("\n\x1b[1m{}\x1b[0m", title);
    println!("{}", "─".repeat(title.chars().count().max(40)));
}

fn docs_only(n: &Node) -> String {
    // The workspace header includes the member count, which travels on the mesh
    // membership channel rather than as a relay blob. Compare document state.
    n.render().lines().skip(1).collect::<Vec<_>>().join("\n")
}

fn ok(msg: &str) {
    println!("  \x1b[32m✓\x1b[0m {msg}");
}
fn info(msg: &str) {
    println!("  · {msg}");
}

fn main() {
    let t0 = Instant::now();
    println!("\x1b[1mMeshDoc, offline-first encrypted collaboration\x1b[0m");
    println!("Demonstration of the core loop: identity → workspace → CRDT → mesh → relay");

    // 1. onboarding
    hr("1. Onboarding: two devices, no server, no account");
    let mut alice = Node::create_workspace(Identity::generate(), "Quake Response").unwrap();
    let bob_identity = Identity::generate();
    let bob_key = bob_identity.public();

    let invite = alice.invite_for(&bob_key).unwrap();
    let token = invite.to_token();
    info(&format!("Alice's workspace id: {}", alice.workspace_id));
    info(&format!(
        "Invite token: {} bytes (fits in a QR code)",
        token.len()
    ));

    let mut bob =
        Node::join_from_invite(bob_identity, &Invite::from_token(&token).unwrap()).unwrap();
    // HQ joins here, then goes offline and does not reconnect until step 7.
    let hq_identity = Identity::generate();
    let hq_invite = alice.invite_for(&hq_identity.public()).unwrap();
    let mut hq = Node::join_from_invite(hq_identity, &hq_invite).unwrap();
    ok(&format!(
        "Bob joined from the token alone: workspace {}, {} members, epoch {}",
        &bob.workspace_id.to_hex()[..8],
        bob.members().len(),
        bob.epoch()
    ));
    assert_eq!(alice.workspace_id, bob.workspace_id);
    assert_eq!(alice.workspace_name(), bob.workspace_name());

    // 2. create data
    hr("2. Structured documents, authored locally");
    let checklist = alice
        .create_doc(DocType::Checklist, "Trapped persons")
        .unwrap();
    let supplies = alice
        .create_doc(DocType::SupplyForm, "Medical needs")
        .unwrap();
    let clinic = alice.create_doc(DocType::GeoPoint, "Field clinic").unwrap();
    alice
        .add_checklist_item(checklist, "Ward 3, two adults, one child")
        .unwrap();
    alice
        .add_checklist_item(checklist, "Rooftop, elderly couple")
        .unwrap();
    alice
        .set_supply_field(supplies, "morphine", "12", "vials")
        .unwrap();
    alice
        .set_location(clinic, 27.7172, 85.3240, "Patan Durbar square", "medical")
        .unwrap();
    alice.sync_with(&mut bob).unwrap();
    alice.sync_with(&mut hq).unwrap();
    ok(&format!(
        "Bob now holds {} document(s) after one sync",
        bob.doc_ids().len()
    ));
    info("HQ has the baseline and now goes offline (it will not sync again until step 7)");

    // 3. partition
    hr("3. Partition: both edit offline, nobody can talk to anybody");
    let items = alice.doc(&checklist).unwrap().iter_items();
    alice
        .toggle_checklist_item(checklist, &items[0].id, true)
        .unwrap();
    alice
        .add_checklist_item(checklist, "Water tanker requested")
        .unwrap();

    bob.add_checklist_item(checklist, "Generator fuel, 20L")
        .unwrap();
    bob.set_supply_field(supplies, "bandages", "40", "packs")
        .unwrap();

    info(&format!(
        "Alice: {} checklist items",
        alice.doc(&checklist).unwrap().iter_items().len()
    ));
    info(&format!(
        "Bob:   {} checklist items",
        bob.doc(&checklist).unwrap().iter_items().len()
    ));
    assert_ne!(
        alice.render(),
        bob.render(),
        "they must disagree while partitioned"
    );
    ok("Divergence confirmed, the two replicas hold different state");

    // 4. convergence
    hr("4. Reconnect: anti-entropy reconciles both ways");
    let (_, sent_a) = alice.sync_with(&mut bob).unwrap();
    let (_, sent_b) = bob.sync_with(&mut alice).unwrap();
    info(&format!(
        "Alice → Bob: {sent_a} change(s); Bob → Alice: {sent_b} change(s)"
    ));
    let converged = alice.render() == bob.render();
    assert!(converged, "replicas must converge");
    ok("Both replicas are byte-identical");
    let (done, total) = alice.doc(&checklist).unwrap().checklist_progress();
    ok(&format!(
        "Checklist merged: {done}/{total} done, {} items total",
        alice.doc(&checklist).unwrap().iter_items().len()
    ));
    assert_eq!(alice.doc(&checklist).unwrap().iter_items().len(), 4);

    // 5. gossip
    hr("5. Epidemic sync: Carol joins and never meets Alice");
    let carol_identity = Identity::generate();
    let carol_invite = bob.invite_for(&carol_identity.public()).unwrap();
    let mut carol = Node::join_from_invite(carol_identity, &carol_invite).unwrap();
    bob.sync_with(&mut carol).unwrap();
    ok(&format!(
        "Carol received {} document(s) via Bob",
        carol.doc_ids().len()
    ));
    // Carol's membership entry itself spreads the same way, Bob carries it back to
    // Alice on the next contact. Alice and Carol still never talk directly.
    bob.sync_with(&mut alice).unwrap();
    alice.sync_with(&mut bob).unwrap();
    assert_eq!(carol.doc(&checklist).unwrap().iter_items().len(), 4);
    assert_eq!(
        carol.render(),
        alice.render(),
        "gossip must reproduce Alice's state"
    );
    ok("Carol's state is identical to Alice's, through a chain of one contact");

    // 6. forgery
    hr("6. Hostile change: an outsider holds the workspace id AND the key");
    let eve = Identity::generate();
    // Worst case: Eve has obtained the real workspace key out of band. Authorisation
    // must still fail, because the key is not what grants write access, the
    // signed membership log is.
    let stolen_key = alice
        .epoch_key(Epoch::GENESIS)
        .expect("alice holds the epoch key");
    let eve_env = ChangeEnvelope::create(
        &eve,
        alice.workspace_id,
        checklist,
        Epoch::GENESIS,
        9999,
        &stolen_key,
        b"forged checklist update",
    )
    .unwrap();
    let before = bob.change_count();
    bob.ingest(eve_env).unwrap();
    ok(&format!(
        "Bob stored the envelope ({} → {} changes) but did not apply it",
        before,
        bob.change_count()
    ));
    assert_eq!(
        bob.doc(&checklist).unwrap().iter_items().len(),
        4,
        "forgery must never apply, even with a valid key"
    );
    ok("Checklist unchanged, Eve's key is not in the signed membership log");

    // 7. relay
    hr("7. Delay-tolerant relay: field pushes, HQ pulls much later");
    let behind_before = hq.change_count();
    let relay_blobs: Vec<Vec<u8>> = alice
        .envelopes_not_in(&Default::default(), usize::MAX)
        .iter()
        .map(|e| e.encode())
        .collect();
    info(&format!(
        "HQ has been offline and is {} change(s) behind",
        alice.change_count() - behind_before
    ));
    info(&format!(
        "Alice uploads {} encrypted blob(s) to a relay that can read none of them",
        relay_blobs.len()
    ));
    // Alice is now offline again. HQ pulls when it next has connectivity.
    let mut applied = 0;
    for blob in &relay_blobs {
        if let Ok(env) = ChangeEnvelope::decode(blob) {
            if hq.ingest(env).unwrap_or(false) {
                applied += 1;
            }
        }
    }
    applied += hq.retry_pending();
    ok(&format!(
        "HQ applied {applied} change(s) pulled from the relay"
    ));
    assert_eq!(hq.doc(&checklist).unwrap().iter_items().len(), 4);
    assert_eq!(
        docs_only(&hq),
        docs_only(&alice),
        "HQ must converge via the relay alone"
    );
    ok("HQ converged to the same documents without ever being online with anyone");
    info("Known gap: membership entries travel on the mesh channel, not as relay blobs,");
    info("so HQ has not yet learned that Carol joined (documented in the README).");

    // 8. summary
    hr("Summary");
    let nodes = [&alice, &hq, &carol, &bob];
    for (i, n) in nodes.iter().enumerate() {
        println!(
            "  replica {i}: {} changes, {} applied, {} pending, {} rejected",
            n.change_count(),
            n.applied_count(),
            n.pending_count(),
            n.rejected_count()
        );
    }
    // Bob holds exactly one rejected change: Eve's forgery, which is the point.
    assert_eq!(
        bob.rejected_count(),
        1,
        "only the forged change is rejected"
    );
    assert!(alice.rejected_count() == 0 && hq.rejected_count() == 0 && carol.rejected_count() == 0);
    // Document state must be identical everywhere. Membership views can lag by one
    // contact (HQ learned of Carol only via relay blobs, which do not carry it).
    assert_eq!(
        docs_only(&alice),
        docs_only(&hq),
        "alice and HQ agree on documents"
    );
    assert_eq!(docs_only(&hq), docs_only(&carol));
    assert_eq!(docs_only(&carol), docs_only(&bob));
    assert_eq!(alice.members().len(), 4, "alice knows all four members");
    assert_eq!(bob.members().len(), 4);
    assert_eq!(carol.members().len(), 4);
    info(&format!(
        "member views: alice={} bob={} carol={} hq={}",
        alice.members().len(),
        bob.members().len(),
        carol.members().len(),
        hq.members().len()
    ));

    println!("\n\x1b[1mFinal document state (identical on every replica)\x1b[0m");
    print!("{}", docs_only(&alice));
    println!(
        "\n\x1b[32mAll assertions passed\x1b[0m in {:.2}s",
        t0.elapsed().as_secs_f64()
    );
}
