# iroh-loro

A peer-to-peer collaborative document editing application that uses
[iroh](https://github.com/n0-computer/iroh) for p2p networking and
[loro](https://github.com/loro-dev/loro) for CRDT-based document synchronization.

## Features

- **Multi-peer collaboration** - Supports unlimited number of connected peers
- **Real-time sync** - Bidirectional document synchronization with broadcast channels
- **Full Loro support** - All container types (Text, Map, List, Tree, MovableList)
- **File watching** - Automatic sync when files are modified externally
- **Persistent keys** - Stable node identity across sessions

## Usage

The demo supports two modes: serving a file (hosting) and joining an existing
session.

### Hosting a File

To start hosting a file for collaborative editing:

```bash
cargo run -r serve <FILE_PATH>
```

This will:

1. Start watching the specified file for changes
2. Print your Node ID that others can use to connect
3. Automatically sync changes with all connected peers in real-time
4. Support multiple simultaneous peer connections

### Joining a Session

To join an existing collaborative editing session:

```bash
cargo run -r join <REMOTE_NODE_ID> <LOCAL_FILE_PATH>
```

Where:

- `REMOTE_NODE_ID` is the Node ID printed by the host
- `LOCAL_FILE_PATH` is where you want to save the synchronized file locally

## Architecture

The application uses a broadcast-based architecture for multi-peer synchronization:

- **Broadcast Channels** - Changes are broadcast to all connected peers simultaneously
- **Bidirectional Sync** - Each peer both sends and receives changes in real-time
- **External Document Access** - Full Loro document operations available via `protocol.doc()`
- **Concurrent Operations** - Uses `tokio::select!` for handling multiple peer connections

## Dependencies

- `iroh 0.91.2` - P2P networking and discovery
- `loro 1.6.1` - CRDT document synchronization
- `rand_core 0.9.3` - Cryptographic random number generation
- `tokio` - Async runtime with full features
