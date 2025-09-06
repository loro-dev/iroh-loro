# Iroh-Loro P2P Text File Synchronization

A peer-to-peer **plain text** file synchronization system built with Iroh networking and Loro CRDTs.

> **⚠️ Important**: This implementation defines `IrohLoroTextProtocol` which is specifically designed for **plain text only** (using Loro's `LoroText` container). It does **not** support rich text formatting hence the use of .txt files. Separate protocols would need to be implemented for other Loro container types. For example, if you want to sync pdfs and word excel documents, you will need to define a `IrohLoroRichTextProtocol`.

## Demo: Real-time Collaborative Text Editing

This demo showcases the power of combining **Iroh's P2P networking** with **Loro's CRDT technology** for seamless real-time collaboration.

### Try It Out!

1. **Start the host peer:**
   ```bash
   cargo run serve demo/test.txt
   ```

2. **Start the client peer:**
   ```bash
   cargo run join <NODE_ID> demo/peer.txt
   ```

3. **Edit both files simultaneously:**
   - `demo/test.txt` starts with content about Iroh's networking capabilities
   - `demo/peer.txt` starts with content about Loro's CRDT technology
   - Watch as your edits merge automatically across peers!

### Add Your Own Content!

Try adding statements like these to either file and watch them sync:

**About Iroh:**
- "Iroh's hole punching is revolutionary!"
- "P2P connections just work with Iroh!"
- "The relay system is so robust!"

**About Loro:**
- "Loro makes CRDTs accessible to everyone!"
- "Conflict-free merging is magical!"
- "The performance is incredible!"

**Combined observations:**
- "Iroh + Loro = the perfect collaboration stack!"
- "P2P CRDTs have never been this easy!"
- "This is the future of distributed applications!"

The system will automatically merge your additions, creating a combined document that showcases both technologies!

## Features

- **Real-time synchronization**: Changes made to files are instantly propagated to all connected peers
- **Eventual consistency**: Automatic periodic consistency checks ensure all online peers converge to the same state
- **Conflict-free editing**: Built on Loro CRDT, ensuring consistent state across all peers
- **File watching**: Automatically detects and syncs file changes
- **Peer-to-peer networking**: Direct connections between peers using Iroh
- **Self-healing sync**: Detects inconsistencies and automatically triggers resync
- **State verification**: SHA256 hashing of document states for reliable consistency checking
- **Loro Text CRDT integration**: Uses Loro's `LoroText` container for conflict-free plain text editing (no rich text support)
- **Persistent keys** - Stable node identity across sessions

## Usage

The demo supports two modes: serving a file (hosting) and joining an existing
session.

### Hosting a File

To start hosting a file for collaborative editing:

```bash
cargo run serve <FILE_PATH>
```

This will:

1. Start watching the specified file for changes
2. Print your Node ID that others can use to connect
3. Automatically sync changes with all connected peers in real-time
4. Support multiple simultaneous peer connections

### Joining a Session

To join an existing collaborative editing session:

```bash
cargo run join <REMOTE_NODE_ID> <LOCAL_FILE_PATH>
```

Where:

- `REMOTE_NODE_ID` is the Node ID printed by the host
- `LOCAL_FILE_PATH` is where you want to save the synchronized file locally

## Architecture

The `IrohLoroTextProtocol` uses a broadcast-based architecture for multi-peer synchronization:

- **Broadcast Channels** - Changes are broadcast to all connected peers simultaneously
- **Bidirectional Sync** - Each peer both sends and receives changes in real-time
- **External Document Access** - Full Loro document operations available via `protocol.doc()`
- **Concurrent Operations** - Uses `tokio::select!` for handling multiple peer connections
- **Plain Text Only** - Uses Loro's `LoroText` container exclusively for plain text synchronization

## Extending to Other Loro Container Types

**This implementation is limited to plain text files.** To support other Loro container types, you would need to implement separate protocols:

### Required Protocols for Full Loro Support

- **`IrohLoroRichTextProtocol`** - For rich text with formatting (bold, italic, links, etc.)
- **`IrohLoroMapProtocol`** - For key-value data structures (JSON-like documents)
- **`IrohLoroListProtocol`** - For ordered lists and arrays
- **`IrohLoroTreeProtocol`** - For hierarchical data structures
- **`IrohLoroMovableListProtocol`** - For reorderable lists

### Why Separate Protocols?

Each Loro container type has different:
- **Serialization formats** (plain text vs JSON vs custom)
- **File extensions** (.txt vs .json vs .xml)
- **Update methods** (text.update() vs map.insert() vs list.push())
- **Conflict resolution semantics** (text merging vs last-write-wins vs operational transforms)

The current `IrohLoroTextProtocol` serves as a foundation that can be adapted for other container types by modifying the serialization and container access layers.

## Dependencies

- `iroh 0.91` - P2P networking and discovery
- `loro 1.6` - CRDT document synchronization (LoroText container only)
- `rand_core 0.9` - Cryptographic random number generation
- `tokio` - Async runtime with full features
