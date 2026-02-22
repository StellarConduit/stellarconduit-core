# StellarConduit Core

> The heart of the StellarConduit protocol — mesh networking engine, gossip protocol, peer discovery, and transaction propagation for offline-first Stellar payments.

This repository contains the core networking layer of StellarConduit. It is responsible for everything that happens between devices in the mesh — discovering peers, establishing connections, propagating signed transaction envelopes through the network using a gossip protocol, and routing messages toward relay nodes for Stellar settlement.

Every other StellarConduit component depends on this library. The mobile app, relay node, and sync engine all build on top of `stellarconduit-core`.

---

## 📋 Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Modules](#modules)
- [Repository Structure](#repository-structure)
- [Prerequisites](#prerequisites)
- [Getting Started](#getting-started)
- [Development](#development)
- [Testing](#testing)
- [Contributing](#contributing)
- [License](#license)

---

## Overview

StellarConduit Core is a Rust library that implements the peer-to-peer mesh networking layer of the StellarConduit protocol. It is designed to run on mobile devices, relay nodes, and any hardware that needs to participate in the StellarConduit mesh.

The core library handles:

- **Peer Discovery** — finding nearby StellarConduit devices via Bluetooth Low Energy advertisements and WiFi-Direct probing
- **Connection Management** — establishing, maintaining, and tearing down connections between peers with automatic reconnection
- **Gossip Protocol** — propagating signed transaction envelopes across the mesh using an epidemic broadcast protocol optimized for low-bandwidth, high-latency environments
- **Message Deduplication** — preventing redundant message retransmission using bloom filters
- **Topology Mapping** — building and maintaining a local view of the mesh network topology to make intelligent routing decisions
- **Relay Discovery** — identifying which peers in the mesh have internet connectivity and routing transaction envelopes toward them
- **Transport Abstraction** — a unified interface over BLE and WiFi-Direct so upper layers do not need to care about the underlying transport

---

## Architecture
```

┌─────────────────────────────────────────────────────────────┐
│                    stellarconduit-core                       │
│                                                             │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │  Discovery  │  │  Transport  │  │   Gossip Protocol   │ │
│  │             │  │             │  │                     │ │
│  │ - BLE scan  │  │ - BLE       │  │ - push/pull         │ │
│  │ - WiFi-D    │  │ - WiFi-D    │  │ - bloom filter      │ │
│  │ - peer list │  │ - unified   │  │ - fanout control    │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
│                                                             │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │  Topology   │  │   Router    │  │      Message        │ │
│  │             │  │             │  │                     │ │
│  │ - mesh map  │  │ - relay     │  │ - envelope schema   │ │
│  │ - hop count │  │   routing   │  │ - signing           │ │
│  │ - peer rank │  │ - path find │  │ - verification      │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
│                                                             │
└─────────────────────────────────────────────────────────────┘
           │                    │                   │
           ▼                    ▼                   ▼
  stellarconduit-      stellarconduit-      stellarconduit-
      mobile            relay-node          sync-engine
```

---

## Modules

### `discovery`
Handles peer discovery over Bluetooth Low Energy and WiFi-Direct. Continuously scans for nearby StellarConduit devices, maintains a list of known peers with their last-seen timestamps and signal strength, and emits events when new peers are found or existing peers go offline.

**Key responsibilities:**
- BLE advertisement broadcasting and scanning
- WiFi-Direct device probing
- Peer list maintenance with TTL-based expiry
- Signal strength based peer ranking
- Discovery event emission to upper layers

---

### `transport`
Provides a unified transport abstraction over BLE and WiFi-Direct. Upper layers send and receive messages without caring about the underlying physical transport. The transport module automatically selects the best available transport for each peer connection and falls back gracefully when a transport becomes unavailable.

**Key responsibilities:**
- BLE GATT server and client implementation
- WiFi-Direct connection establishment and teardown
- Transport selection and fallback logic
- Connection state management
- Automatic reconnection with exponential backoff
- Message framing and chunking for large payloads

---

### `gossip`
Implements the epidemic gossip protocol that propagates signed transaction envelopes across the mesh. When a device receives a new transaction envelope, it forwards it to a selected subset of its peers. This process repeats until the message has reached every device in the mesh, or until it reaches a relay node and is settled on Stellar.

The gossip protocol is specifically tuned for the StellarConduit use case — extremely low bandwidth, high latency, and unreliable connections. Message sizes are minimized, redundant transmissions are suppressed using bloom filters, and the fanout factor is dynamically adjusted based on mesh density.

**Key responsibilities:**
- Push-based gossip for new transaction envelopes
- Pull-based reconciliation for recovering missed messages
- Bloom filter based deduplication
- Dynamic fanout based on peer count and mesh density
- Message TTL and expiry
- Gossip round scheduling

---

### `topology`
Builds and maintains a local view of the mesh network topology. Each device tracks its known peers, their connections to each other, estimated hop counts to relay nodes, and overall mesh health. This topology map is used by the router to make intelligent forwarding decisions.

**Key responsibilities:**
- Local topology graph construction and maintenance
- Hop count estimation to known relay nodes
- Peer connectivity tracking
- Topology change event emission
- Mesh health scoring
- Stale topology pruning

---

### `router`
Uses the topology map to make intelligent routing decisions for outgoing messages. Rather than naively flooding all peers, the router prioritizes forwarding transaction envelopes toward peers that are closer to known relay nodes, reducing unnecessary transmissions and improving settlement speed.

**Key responsibilities:**
- Relay-aware message routing
- Next-hop selection algorithm
- Path finding toward relay nodes
- Fallback to flood when topology is unknown
- Routing table maintenance

---

### `message`
Defines the message and envelope schemas used throughout the StellarConduit protocol. All messages exchanged between peers are typed, versioned, and cryptographically signed. This module also handles message serialization, deserialization, signing, and signature verification.

**Key responsibilities:**
- Message type definitions (transaction envelope, topology update, relay announcement, acknowledgement)
- Message serialization and deserialization (using MessagePack for compactness)
- Cryptographic message signing
- Signature verification
- Message versioning for protocol upgrades
- Relay chain proof construction and verification

---

### `peer`
Represents a peer device in the mesh. Tracks all known information about a peer including their public key, supported transports, relay status, connection history, and reputation score. The peer module is the central data structure that discovery, transport, gossip, and topology all interact with.

**Key responsibilities:**
- Peer data model and identity
- Peer reputation scoring
- Relay capability tracking
- Connection history
- Peer serialization for topology sharing

---

## Repository Structure
```

stellarconduit-core/
├── src/
│   ├── lib.rs
│   ├── discovery/
│   │   ├── mod.rs
│   │   ├── ble.rs
│   │   ├── wifi_direct.rs
│   │   ├── peer_list.rs
│   │   └── events.rs
│   ├── transport/
│   │   ├── mod.rs
│   │   ├── ble_transport.rs
│   │   ├── wifi_transport.rs
│   │   ├── unified.rs
│   │   ├── connection.rs
│   │   └── errors.rs
│   ├── gossip/
│   │   ├── mod.rs
│   │   ├── protocol.rs
│   │   ├── bloom.rs
│   │   ├── fanout.rs
│   │   ├── round.rs
│   │   └── errors.rs
│   ├── topology/
│   │   ├── mod.rs
│   │   ├── graph.rs
│   │   ├── hop_counter.rs
│   │   ├── health.rs
│   │   └── events.rs
│   ├── router/
│   │   ├── mod.rs
│   │   ├── relay_router.rs
│   │   ├── path_finder.rs
│   │   └── table.rs
│   ├── message/
│   │   ├── mod.rs
│   │   ├── types.rs
│   │   ├── envelope.rs
│   │   ├── signing.rs
│   │   └── relay_proof.rs
│   └── peer/
│       ├── mod.rs
│       ├── peer.rs
│       ├── reputation.rs
│       └── identity.rs
├── tests/
│   ├── discovery_test.rs
│   ├── gossip_test.rs
│   ├── topology_test.rs
│   ├── router_test.rs
│   ├── message_test.rs
│   └── integration/
│       ├── mesh_propagation_test.rs
│       ├── relay_routing_test.rs
│       └── double_spend_simulation_test.rs
├── benches/
│   ├── gossip_bench.rs
│   └── bloom_filter_bench.rs
├── examples/
│   ├── basic_mesh.rs
│   └── relay_node_example.rs
├── docs/
│   ├── gossip-protocol.md
│   ├── transport-layer.md
│   └── message-format.md
├── Cargo.toml
├── .gitignore
├── CONTRIBUTING.md
├── LICENSE
└── README.md
```

---

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) `>=1.74.0`
- Linux or macOS for development (Windows support is planned)
- For BLE testing: a machine or device with Bluetooth hardware
- For WiFi-Direct testing: two physical Android devices or a supported Linux setup

Verify your Rust installation:
```bash
rustc --version
cargo --version
```

---

## Getting Started

### 1. Clone the Repository
```bash
git clone https://github.com/StellarConduit/stellarconduit-core.git
cd stellarconduit-core
```

### 2. Build the Library
```bash
cargo build
```

### 3. Run Tests
```bash
cargo test
```

### 4. Run the Basic Mesh Example
```bash
cargo run --example basic_mesh
```

---

## Development

### Running a Specific Module's Tests
```bash
cargo test discovery
cargo test gossip
cargo test topology
```

### Running Benchmarks
```bash
cargo bench
```

### Linting and Formatting

Always run these before submitting a pull request:
```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

### Running the Mesh Simulator

Since most contributors won't have multiple physical Bluetooth devices available, we provide a mesh network simulator that simulates multiple virtual nodes communicating over a local channel. This is the primary development and testing tool for the gossip protocol and topology modules.
```bash
cargo run --example basic_mesh -- --nodes 10 --relay-nodes 2
```

---

## Testing

StellarConduit Core has three levels of testing:

**Unit tests** live alongside the source code in each module and test individual functions and data structures in isolation.

**Integration tests** in the `tests/` directory test how modules interact with each other — for example, testing that a message signed in the `message` module is correctly propagated through the `gossip` module and routed by the `router` module.

**Simulation tests** in `tests/integration/` spin up virtual mesh networks with configurable numbers of nodes, relay nodes, and network conditions (latency, packet loss, partitions) to test protocol behavior at scale. These are the most important tests for validating the gossip protocol and conflict resolution logic.
```bash
# All tests
cargo test

# Unit tests only
cargo test --lib

# Integration tests only
cargo test --test '*'

# Simulation tests
cargo test --test mesh_propagation_test
cargo test --test double_spend_simulation_test
```

We target a minimum of **85% test coverage** for this repository given its critical role in the protocol.

---

## Contributing

StellarConduit Core is the most technically challenging repository in the organization. We especially welcome contributors with experience in:

- Peer-to-peer networking protocols
- Bluetooth Low Energy development
- WiFi-Direct on Android or Linux
- Gossip protocols and epidemic broadcast
- Rust systems programming
- Network simulation and testing

That said, there are also many good first issues for contributors who are newer to these areas. Browse issues labeled [`good first issue`](https://github.com/StellarConduit/stellarconduit-core/issues?q=label%3A%22good+first+issue%22) to get started.

Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

---

## License

This repository is licensed under the [Apache 2.0 License](LICENSE).

---

<div align="center">

Part of the [StellarConduit](https://github.com/StellarConduit) open-source organization.

**Payments that work everywhere. Even where the internet doesn't.**

</div>
