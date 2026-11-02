//! `meshdoc`, the command-line client.
//!
//! It is a real client: every command loads encrypted state from disk, mutates it,
//! and writes it back sealed. Nothing is kept in memory between invocations, which
//! is exactly how the Android client behaves on process death.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use meshdoc::crypto::Identity;
use meshdoc::docs::DocType;
use meshdoc::ids::{DocId, WorkspaceId};
use meshdoc::node::{Invite, Node};
use meshdoc::relay::RelayClient;
use meshdoc::store::Store;
use meshdoc::sync;
use meshdoc::transport::TcpMesh;

#[derive(Parser)]
#[command(
    name = "meshdoc",
    version,
    about = "Offline-first, end-to-end-encrypted collaboration for teams without reliable connectivity",
    long_about = None
)]
struct Cli {
    /// Directory holding the encrypted store
    #[arg(long, global = true, default_value = "./.meshdoc", env = "MESHDOC_DIR")]
    dir: PathBuf,

    /// Passphrase protecting the store (or set MESHDOC_PASSPHRASE)
    #[arg(
        long,
        global = true,
        env = "MESHDOC_PASSPHRASE",
        hide_env_values = true
    )]
    pass: Option<String>,

    /// Workspace to operate on (defaults to the only one in the store)
    #[arg(long, global = true)]
    ws: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new device identity in the store
    Init,
    /// Show this device's identity
    Id {
        /// Print only the public identity (128 hex chars), for scripts and QR codes
        #[arg(long)]
        hex: bool,
    },
    /// Workspace operations
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// Document operations
    Doc {
        #[command(subcommand)]
        cmd: DocCmd,
    },
    /// Checklist operations
    Checklist {
        #[command(subcommand)]
        cmd: ChecklistCmd,
    },
    /// Supply-form operations
    Supply {
        #[command(subcommand)]
        cmd: SupplyCmd,
    },
    /// Geo-point operations
    Geo {
        #[command(subcommand)]
        cmd: GeoCmd,
    },
    /// Note operations
    Note {
        #[command(subcommand)]
        cmd: NoteCmd,
    },
    /// Peer-to-peer sync over TCP (Wi-Fi / LAN)
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
    },
    /// Store-and-forward relay (the optional internet path)
    Relay {
        #[command(subcommand)]
        cmd: RelayCmd,
    },
    /// Print the whole workspace state deterministically
    Render,
    /// Print a one-line status summary
    Status,
}

#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Create a workspace; you become the founding member
    Create {
        #[arg(long)]
        name: String,
    },
    /// List workspaces in the store
    List,
    /// Show workspace details
    Show,
    /// Mint an invite token for a joining device's public identity
    Invite {
        /// The joining device's public identity (from `meshdoc id --hex`)
        #[arg(long = "for")]
        for_identity: String,
    },
    /// Join a workspace from an invite token
    Join {
        #[arg(long)]
        token: String,
    },
}

#[derive(Subcommand)]
enum DocCmd {
    /// Create a document
    Create {
        #[arg(long, value_parser = parse_doctype)]
        r#type: DocType,
        #[arg(long)]
        title: String,
    },
    /// List documents
    List,
    /// Show a document
    Show { id: String },
}

#[derive(Subcommand)]
enum ChecklistCmd {
    /// Add an item to a checklist
    Add { doc: String, text: String },
    /// Tick or untick an item
    Toggle {
        doc: String,
        item: String,
        #[arg(long)]
        undone: bool,
    },
    /// Remove an item
    Remove { doc: String, item: String },
}

#[derive(Subcommand)]
enum SupplyCmd {
    /// Set a field on a supply form
    Set {
        doc: String,
        field: String,
        value: String,
        #[arg(long, default_value = "")]
        unit: String,
    },
}

#[derive(Subcommand)]
enum GeoCmd {
    /// Set the location of a geo point
    Set {
        doc: String,
        lat: f64,
        lon: f64,
        #[arg(long, default_value = "")]
        label: String,
        #[arg(long, default_value = "landmark")]
        kind: String,
    },
}

#[derive(Subcommand)]
enum NoteCmd {
    /// Replace the note body
    Set { doc: String, text: String },
    /// Append to the note body
    Append { doc: String, text: String },
}

#[derive(Subcommand)]
enum RelayCmd {
    /// Check that the relay is reachable and report its state
    Status { relay: String },
    /// Upload every change we know to the relay
    Push { relay: String },
    /// Download changes from the relay and apply them
    Pull { relay: String },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Pull changes from a peer (they must be running `sync serve`)
    Pull { peer: SocketAddr },
    /// Serve sync sessions to incoming peers
    Serve {
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Exit after this many sessions (0 = run forever)
        #[arg(long, default_value_t = 1)]
        max_sessions: usize,
    },
}

fn parse_doctype(s: &str) -> Result<DocType, String> {
    DocType::parse(s)
        .ok_or_else(|| format!("unknown type '{s}' (checklist|supply_form|geo_point|note)"))
}

fn parse_docid(s: &str) -> Result<DocId, String> {
    // Accept either a full 32-hex id or an 8-hex prefix (as printed by `doc list`).
    let s = s.trim();
    if s.len() == 32 {
        return DocId::from_hex(s).ok_or_else(|| "malformed document id".to_string());
    }
    Err(format!(
        "'{s}' is not a full document id (use the 32-hex id from `doc list`)"
    ))
}

fn parse_wsid(s: &str) -> Result<WorkspaceId, String> {
    WorkspaceId::from_hex(s.trim()).ok_or_else(|| "malformed workspace id".to_string())
}

fn pass_of(cli: &Cli) -> String {
    cli.pass
        .clone()
        .or_else(|| std::env::var("MESHDOC_PASSPHRASE").ok())
        .unwrap_or_else(|| {
            eprintln!("warning: no passphrase supplied (--pass or MESHDOC_PASSPHRASE); using an empty one");
            String::new()
        })
}

/// Load the identity and the selected node from the store.
fn load(cli: &Cli) -> Result<(Store, Node), String> {
    let pass = pass_of(cli);
    let store = Store::open(&cli.dir).map_err(|e| e.to_string())?;
    let identity = store.load_identity(&pass).map_err(|e| e.to_string())?;
    let ws = match &cli.ws {
        Some(w) => parse_wsid(w)?,
        None => {
            let list = store.list_workspaces().map_err(|e| e.to_string())?;
            match list.len() {
                0 => {
                    return Err(
                        "no workspace in this store (run `workspace create --name ...`)".into(),
                    )
                }
                1 => list[0],
                _ => {
                    return Err(format!(
                        "store holds {} workspaces; pass --ws <id> (see `workspace list`)",
                        list.len()
                    ))
                }
            }
        }
    };
    let node = store
        .load_node(identity, ws, &pass)
        .map_err(|e| e.to_string())?;
    Ok((store, node))
}

fn save(cli: &Cli, store: &Store, node: &Node) -> Result<(), String> {
    let pass = pass_of(cli);
    store.save_node(node, &pass).map_err(|e| e.to_string())
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match &cli.cmd {
        Cmd::Init => {
            let pass = pass_of(&cli);
            let store = Store::open(&cli.dir).map_err(|e| e.to_string())?;
            if store.has_identity() {
                return Err("a device identity already exists in this store".into());
            }
            let id = Identity::generate();
            store.save_identity(&id, &pass).map_err(|e| e.to_string())?;
            println!("created device identity");
            println!("  fingerprint : {}", id.fingerprint());
            println!("  public key  : {}", id.public().to_hex());
            println!("  store       : {}", store.dir().display());
            Ok(())
        }
        Cmd::Id { hex } => {
            let pass = pass_of(&cli);
            let store = Store::open(&cli.dir).map_err(|e| e.to_string())?;
            let id = store.load_identity(&pass).map_err(|e| e.to_string())?;
            if *hex {
                println!("{}", id.public().to_hex());
            } else {
                println!("fingerprint : {}", id.fingerprint());
                println!("public key  : {}", id.public().to_hex());
                println!("peer id     : {}", id.fingerprint());
            }
            Ok(())
        }
        Cmd::Workspace { cmd } => workspace_cmd(&cli, cmd),
        Cmd::Doc { cmd } => doc_cmd(&cli, cmd),
        Cmd::Checklist { cmd } => checklist_cmd(&cli, cmd),
        Cmd::Supply { cmd } => supply_cmd(&cli, cmd),
        Cmd::Geo { cmd } => geo_cmd(&cli, cmd),
        Cmd::Note { cmd } => note_cmd(&cli, cmd),
        Cmd::Sync { cmd } => sync_cmd(&cli, cmd),
        Cmd::Relay { cmd } => relay_cmd(&cli, cmd),
        Cmd::Render => {
            let (_s, node) = load(&cli)?;
            print!("{}", node.render());
            Ok(())
        }
        Cmd::Status => {
            let (store, node) = load(&cli)?;
            println!("{}", node.summary());
            println!("store       : {}", store.dir().display());
            println!("applied     : {}", node.applied_count());
            println!("pending     : {}", node.pending_count());
            println!("stored bytes: {}", node.stored_bytes());
            Ok(())
        }
    }
}

fn workspace_cmd(cli: &Cli, cmd: &WorkspaceCmd) -> Result<(), String> {
    let pass = pass_of(cli);
    let store = Store::open(&cli.dir).map_err(|e| e.to_string())?;
    match cmd {
        WorkspaceCmd::Create { name } => {
            let id = if store.has_identity() {
                store.load_identity(&pass).map_err(|e| e.to_string())?
            } else {
                let id = Identity::generate();
                store.save_identity(&id, &pass).map_err(|e| e.to_string())?;
                id
            };
            let node = Node::create_workspace(id, name).map_err(|e| e.to_string())?;
            store.save_node(&node, &pass).map_err(|e| e.to_string())?;
            println!("created workspace \"{name}\"");
            println!("  workspace id : {}", node.workspace_id);
            println!("  epoch        : {}", node.epoch());
            Ok(())
        }
        WorkspaceCmd::List => {
            for ws in store.list_workspaces().map_err(|e| e.to_string())? {
                let id = store.load_identity(&pass).map_err(|e| e.to_string())?;
                match store.load_node(id, ws, &pass) {
                    Ok(n) => println!(
                        "{}  \"{}\"  {} docs",
                        ws,
                        n.workspace_name(),
                        n.doc_ids().len()
                    ),
                    Err(_) => println!("{ws}  <unreadable>"),
                }
            }
            Ok(())
        }
        WorkspaceCmd::Show => {
            let (_s, node) = load(cli)?;
            println!("workspace   : {}", node.workspace_id);
            println!("name        : {}", node.workspace_name());
            println!("epoch       : {}", node.epoch());
            println!("members     : {}", node.members().len());
            for m in node.members() {
                println!("  member    : {m}");
            }
            println!("documents   : {}", node.doc_ids().len());
            println!("changes     : {}", node.change_count());
            Ok(())
        }
        WorkspaceCmd::Invite { for_identity } => {
            let (_s, mut node) = load(cli)?;
            let joiner = meshdoc::crypto::PublicIdentity::from_hex(for_identity)
                .ok_or("--for expects a 128-hex public identity (from `meshdoc id --hex` on the joining device)")?;
            let invite = node.invite_for(&joiner).map_err(|e| e.to_string())?;
            save(cli, &_s, &node)?;
            println!("{}", invite.to_token());
            Ok(())
        }
        WorkspaceCmd::Join { token } => {
            let invite = Invite::from_token(token).map_err(|e| e.to_string())?;
            let id = if store.has_identity() {
                store.load_identity(&pass).map_err(|e| e.to_string())?
            } else {
                return Err("no device identity: run `meshdoc init` first".into());
            };
            let node = Node::join_from_invite(id, &invite).map_err(|e| e.to_string())?;
            store.save_node(&node, &pass).map_err(|e| e.to_string())?;
            println!("joined workspace {}", node.workspace_id);
            println!("  name    : {}", node.workspace_name());
            println!("  members : {}", node.members().len());
            Ok(())
        }
    }
}

fn doc_cmd(cli: &Cli, cmd: &DocCmd) -> Result<(), String> {
    let (store, mut node) = load(cli)?;
    match cmd {
        DocCmd::Create { r#type, title } => {
            let did = node.create_doc(*r#type, title).map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!("{did}");
            Ok(())
        }
        DocCmd::List => {
            if node.doc_ids().is_empty() {
                println!("(no documents)");
            }
            for did in node.doc_ids() {
                if let Some(d) = node.doc(&did) {
                    let extra = match d.ty() {
                        DocType::Checklist => {
                            let (done, total) = d.checklist_progress();
                            format!("{done}/{total} done")
                        }
                        DocType::SupplyForm => format!("{} fields", d.iter_fields().len()),
                        DocType::GeoPoint => {
                            let g = d.location();
                            format!("({:.4}, {:.4}) {}", g.lat, g.lon, g.label)
                        }
                        DocType::Note => format!("{} chars", d.text().chars().count()),
                    };
                    println!(
                        "{}  {:<12} {:<28} {}",
                        did,
                        d.ty().as_str(),
                        d.title(),
                        extra
                    );
                }
            }
            Ok(())
        }
        DocCmd::Show { id } => {
            let did = parse_docid(id)?;
            let doc = node
                .doc(&did)
                .ok_or_else(|| format!("no such document {id}"))?;
            print!("{}", doc.render());
            Ok(())
        }
    }
}

fn checklist_cmd(cli: &Cli, cmd: &ChecklistCmd) -> Result<(), String> {
    let (store, mut node) = load(cli)?;
    match cmd {
        ChecklistCmd::Add { doc, text } => {
            let did = parse_docid(doc)?;
            let (_cid, item) = node
                .add_checklist_item_id(did, text)
                .map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!("{item}");
            Ok(())
        }
        ChecklistCmd::Toggle { doc, item, undone } => {
            let did = parse_docid(doc)?;
            node.toggle_checklist_item(did, item, !undone)
                .map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!("{}", if *undone { "unticked" } else { "ticked" });
            Ok(())
        }
        ChecklistCmd::Remove { doc, item } => {
            let did = parse_docid(doc)?;
            let d = node.doc(&did).ok_or("no such document")?;
            let ok = d.remove_item(item);
            if ok {
                save(cli, &store, &node)?;
                println!("removed");
            } else {
                return Err("no such item".into());
            }
            Ok(())
        }
    }
}

fn supply_cmd(cli: &Cli, cmd: &SupplyCmd) -> Result<(), String> {
    let (store, mut node) = load(cli)?;
    match cmd {
        SupplyCmd::Set {
            doc,
            field,
            value,
            unit,
        } => {
            let did = parse_docid(doc)?;
            node.set_supply_field(did, field, value, unit)
                .map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!("set {field} = {value} {unit}");
            Ok(())
        }
    }
}

fn geo_cmd(cli: &Cli, cmd: &GeoCmd) -> Result<(), String> {
    let (store, mut node) = load(cli)?;
    match cmd {
        GeoCmd::Set {
            doc,
            lat,
            lon,
            label,
            kind,
        } => {
            let did = parse_docid(doc)?;
            node.set_location(did, *lat, *lon, label, kind)
                .map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!("location set to ({lat}, {lon}) {label} [{kind}]");
            Ok(())
        }
    }
}

fn note_cmd(cli: &Cli, cmd: &NoteCmd) -> Result<(), String> {
    let (store, mut node) = load(cli)?;
    match cmd {
        NoteCmd::Set { doc, text } => {
            let did = parse_docid(doc)?;
            node.set_note(did, text).map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!("note saved");
            Ok(())
        }
        NoteCmd::Append { doc, text } => {
            let did = parse_docid(doc)?;
            let existing = node.doc(&did).ok_or("no such document")?.text();
            node.set_note(did, &format!("{existing}{text}"))
                .map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!("note appended");
            Ok(())
        }
    }
}

fn sync_cmd(cli: &Cli, cmd: &SyncCmd) -> Result<(), String> {
    match cmd {
        SyncCmd::Pull { peer } => {
            let (store, mut node) = load(cli)?;
            let mut stream = TcpMesh::connect(*peer).map_err(|e| e.to_string())?;
            let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
            let stats =
                sync::pull(&mut reader, &mut stream, &mut node).map_err(|e| e.to_string())?;
            save(cli, &store, &node)?;
            println!(
                "pulled {} changes ({} applied) in {} page(s)",
                stats.received, stats.applied, stats.pages
            );
            println!("{}", node.summary());
            Ok(())
        }
        SyncCmd::Serve {
            bind,
            port,
            max_sessions,
        } => {
            let (store, mut node) = load(cli)?;
            let mesh = TcpMesh::bind(*port).map_err(|e| e.to_string())?;
            let addr = mesh.local_addr();
            println!(
                "serving sync on {bind}:{} (workspace {})",
                addr.port(),
                &node.workspace_id.to_hex()[..8]
            );

            let mut served = 0usize;
            loop {
                let stream = match mesh.accept_one() {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("accept failed: {e}");
                        continue;
                    }
                };
                let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
                let mut writer = stream;
                match sync::serve(&mut reader, &mut writer, &mut node) {
                    Ok(stats) => {
                        served += 1;
                        println!("session {served}: sent {} changes", stats.sent);
                        save(cli, &store, &node)?;
                    }
                    Err(e) => eprintln!("session error: {e}"),
                }
                if *max_sessions != 0 && served >= *max_sessions {
                    break;
                }
            }
            println!("served {served} session(s)");
            Ok(())
        }
    }
}

fn relay_cmd(cli: &Cli, cmd: &RelayCmd) -> Result<(), String> {
    match cmd {
        RelayCmd::Status { relay } => {
            let client = RelayClient::new(relay).map_err(|e| e.to_string())?;
            let h = client.health().map_err(|e| e.to_string())?;
            let (_store, node) = load(cli)?;
            println!("relay    : {relay}");
            println!("status   : {} (v{})", h.status, h.version);
            println!("blobs    : {} total on relay", h.blobs);
            if let Ok(head) = client.head(node.workspace_id) {
                println!(
                    "workspace: {} blob(s) at seq {head}",
                    &node.workspace_id.to_hex()[..8]
                );
            }
            Ok(())
        }
        RelayCmd::Push { relay } => {
            let client = RelayClient::new(relay).map_err(|e| e.to_string())?;
            let (_store, node) = load(cli)?;
            let all = node.envelopes_not_in(&Default::default(), usize::MAX);
            let blobs: Vec<Vec<u8>> = all.iter().map(|e| e.encode()).collect();
            let sent = client
                .push_changes(node.workspace_id, &blobs)
                .map_err(|e| e.to_string())?;
            println!("pushed {sent} new change(s) to {relay}");
            println!(
                "relay head is now {}",
                client.head(node.workspace_id).unwrap_or(0)
            );
            Ok(())
        }
        RelayCmd::Pull { relay } => {
            let client = RelayClient::new(relay).map_err(|e| e.to_string())?;
            let (store, mut node) = load(cli)?;
            let blobs = client
                .fetch_since(node.workspace_id, 0)
                .map_err(|e| e.to_string())?;
            let mut applied = 0usize;
            let mut seen = 0usize;
            for b in &blobs {
                if let Ok(env) = meshdoc::change::ChangeEnvelope::decode(&b.data) {
                    seen += 1;
                    if node.ingest(env).unwrap_or(false) {
                        applied += 1;
                    }
                }
            }
            applied += node.retry_pending();
            save(cli, &store, &node)?;
            println!(
                "fetched {} blob(s), {seen} change(s), {applied} newly applied",
                blobs.len()
            );
            println!("{}", node.summary());
            Ok(())
        }
    }
}
