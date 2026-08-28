# flyco

Agentic coding on the web, on your own cloud. Flyco runs the official coding harnesses — Claude Code and Codex — on ephemeral VMs you pay for, driven from a browser-first PWA, with compute budgets the agent manages itself.

- **Product spec**: [docs/proposal.md](docs/proposal.md)
- **Technical design**: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
- **Harness feature parity**: [docs/feature-matrix.md](docs/feature-matrix.md)

Built with [skyzen](https://crates.io/crates/skyzen) on Cloudflare Workers (control plane) and a native Rust daemon (`flycod`) on session VMs. SolidJS frontend. MIT OR Apache-2.0.
