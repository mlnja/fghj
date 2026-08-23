# Technical Specification: fghj (MVP)

## 1. Overview & Vision
**fghj** is a hyper-focused Local Development Orchestration and Workspace Management tool designed to eliminate the microservices testing crisis. Instead of forcing developers to manage massive, static Docker Compose files or spin up resource-heavy local Kubernetes clusters (`Kind`/`Minikube`), `fghj` shifts the paradigm from **infrastructure-centric** to **product-centric** orchestration. 

Developers spin up isolated local environments based on specific **User Flows** (e.g., `checkout-flow`, `auth-flow`). The tool recursively manages Git dependencies, resolves local port conflicts seamlessly via an OS-level Magic DNS, and enforces production-like local environments using automated, trusted SSL certificates.

---

## 2. Key Architecture Concepts
*   **The `fghj` Superdaemon:** A background system service running with root/administrator privileges. It acts as the local DNS server, reverse proxy, NAT router, and SSL certificate authority.
*   **User Flow:** A functional business scope mapping out exactly which microservices are required to test a specific user journey.
*   **Federated Configuration:** No single mega-repo config. Every repository contains its own `fghj.yaml` file declaring its specific metadata, local domains, environment templates, and immediate downstream dependencies.

---

## 3. High-Level Architecture Diagram
```
                     [ Developer Laptop / Browser ]
                                   │
                    Request to *.fghj.internal (HTTPS)
                                   │
                                   ▼
             ┌───────────────────────────────────────────┐
             │            fghj Superdaemon               │
             │  (Listens on 127.0.0.1:5353 & 127.0.0.1)  │
             └───────────────────┬───────────────────────┘
                                 │
                 ┌───────────────┴───────────────┐
                 ▼                               ▼
        [ Built-in DNS Server ]       [ Reverse Proxy + Root CA ]
        Resolves domain names to     Terminates TLS via custom certs,
        internal Docker network IPs   proxies traffic to Docker containers
                 │                               │
                 └───────────────┬───────────────┘
                                 │
                                 ▼
                     [ Docker Bridge Network ]
               ┌───────────────────┴───────────────────┐
               ▼                                       ▼
       [ container-1: auth ]                   [ container-2: cart ]
```

---

## 4. Configuration Schema (`fghj.yaml`)

### Root Repository Config Example
```yaml
version: "1.0"
workspace_name: "enterprise-platform"

flows:
  checkout-flow:
    description: "Launches all services needed to test the customer checkout journey"
    dependencies:
      - repo: "git@github.com:company/cart-service.git"
        default_branch: "main"
        local_path: "./services/cart"
      - repo: "git@github.com:company/payment-service.git"
        default_branch: "develop"
        local_path: "./services/payment"

  auth-flow:
    description: "Lightweight scope focusing purely on authentication & session management"
    dependencies:
      - repo: "git@github.com:company/auth-service.git"
        default_branch: "main"
        local_path: "./services/auth"
```

### Component Repository Config Example (e.g., inside `cart-service`)
```yaml
version: "1.0"
service:
  name: "cart-service"
  internal_domain: "cart.fghj.internal"
  build:
    context: "."
    dockerfile: "Dockerfile"
  ports:
    - "8080" # Base internal port, mapping happens dynamically via proxy
  env:
    - DB_HOST=postgres.fghj.internal
    - AUTH_API_URL=https://auth.fghj.internal
  dependencies:
    - repo: "git@github.com:company/auth-service.git"
      default_branch: "main"
      local_path: "../auth"
```

---

## 5. Subsystem Specifications

### Subsystem A: Git Dependency Resolver & Graph Engine
*   **Execution Flow:** When a user initializes a flow, `fghj` recursively traces dependencies. If Repo A requires Repo B, and Repo B requires Repo C, `fghj` fetches all three.
*   **Circular Dependency Protection:** The resolver maintains a global in-memory `VisitedRepositories` HashSet containing target repository SSH clones. If an item already exists in the set, the recursion path breaks immediately to safely resolve cyclic graphs.
*   **Git State Guard:** When changing or syncing branches, `fghj` checks local directory state via `git status --porcelain`. If unstaged or uncommitted changes exist, the engine aborts the switch and triggers a CLI interactive prompt offering to either `stash`, `commit`, or `abort` to preserve the user's unsaved code.

### Subsystem B: Split-DNS & Operating System Routing
*   **Local Nameserver Isolation:** The superdaemon spins up a lightweight DNS server listening exclusively on loopback `127.0.0.1:5353`.
*   **Native OS Integration (Zero-Overhead):**
    *   *macOS:* Generates an configuration file under `/etc/resolver/fghj.internal` pointing nameserver routing directly to `127.0.0.1:5353`.
    *   *Linux:* Integrates with `systemd-resolved` by binding a link-specific routing path filtering for the `fghj.internal` domain string.
    *   *Windows:* Registers a temporary Name Resolution Policy Table (NRPT) namespace mapping rule using PowerShell commands.
*   **The Port-Conflict Solution:** The DNS engine dynamically cross-references active Docker container bridge interface IPs. Domains route directly to distinct local network IPs or container targets rather than sharing `localhost`, making collision-free parallel executions of standard database ports (like `5432` or `6379`) possible out of the box.

### Subsystem C: Local Automated Root CA & TLS Reverse Proxy
*   **Certificate Generation:** Upon initial service startup (`fghj setup`), the engine automatically builds a completely unique, locally scoped cryptographic Root Certificate Authority (Root CA).
*   **System Trust Injection:** The system automatically implants the generated Root CA directly into the underlying platform trusted keychains (`/Library/Keychains/System.keychain` on macOS, NSS shared databases for browsers, and the local Windows Root Certificate Storage).
*   **On-The-Fly Server Certificates:** As containers spin up, the embedded proxy issues valid TLS server certificates matching the declared `internal_domain` values (e.g., `https://api.fghj.internal`). It hosts a built-in reverse proxy routing TLS traffic directly into unexposed container ports, delivering a seamless native HTTPS experience across local browsers and HTTP clients.

---

## 6. CLI Command Architecture & User Experience (UX)

The interactive terminal workflows are built for speed and minimal keypresses.

*   `fghj setup`: Installs the root background daemon, generates local CA certificates, and injects platform split-DNS configuration hooks. (Requires `sudo` elevation once).
*   `fghj init <repo_url>`: Fetches the root layout configuration file, bootstraps workspace maps, and scans user flow options.
*   `fghj up`: Launces an interactive terminal UI workflow selector. The developer picks a user flow using arrow keys.
*   `fghj branch`: Displays a real-time branch matrices mapping all active repositories in the local workspace.
*   `fghj branch set <service_name> <branch_name>`: Interactively queries remote branches, verifies dirty status, checks out targeted code variations, and performs hot-reloading configurations for affected runtime instances.

## 7. Recommended Implementation Stack
*   **Core Binary Engine:** Go (Golang) or Rust. Compiling down to a standalone, zero-dependency native distribution binary maximizes portability and aligns perfectly with `curl | bash` bootstrap distribution patterns.
*   **Docker Interface:** Native programmatic bindings or clean binary wrapper calls executing standard Docker CLI / Docker Compose V2 interfaces under the hood.