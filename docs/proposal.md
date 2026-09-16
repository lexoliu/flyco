# Flyco proposal

Flyco.dev is an open-sourced project under the MIT license, aiming to provide agentic coding on the web experience for users, with flexible cloud computing.

## Techstack

We use `skyzen` framework by Lexo Liu(me). This framework allow us to build frontend projects running on serverless platform with Rust.

Also, for http request, use `zenwave` framework by me as well. Just for fun.

Frontend: SolidJS

Frontend toolchain: Bun + Vite + OXC Rust compiler + Vitest

## Platform

Cloudflare serverless. Yes it is the only platform we plan to support, though`skyzen` are designed to support multiple platforms but i personally love cloudflare and want to save time.

## Harness

We only support the two harness and we aim to provide ALL features that available in their official apps.

We do not build our own harness forever.

The two harness is:

- Claude code
- Codex

We should support:

- usage display(for claude, they have indiviual fable usage, for codex, they have indiviual gpt-5.3-codex-spark usage)
- context window display
- goal mode
- Auto mode (by default)
- Btw(by the way)/Side chat
- Ultra mode / dynamic workflows
- Any setting
- Compact
- Advisor
- Monitor
- Background tasks

We should disable:

- Remote control
- Resume

We should takeover:

- Skill
- MCP
- Browser control
- Computer control
- Memory(we should provide MCP tool instead of using the model native memory system, since their memory system is based on files system. By the way, our memory system should also be a tree memory system.

We provide:

- Auto continue with usage limit reset

## Core features

### Computing budgets for agents

User can set up computing budgets for agents. And the agent will decide how to use this budget. Agent will get a notice when they used 50% of budgets, and a warn when they used 80% budgets. When the agent used up 90% budgets. They will get a final WARN. If they used up all budgets. They will be paused immediately.

> If the total budget agent spent is under $5. No warn will be sent until they used up 90% budgets.

Agent can call tool to upgrade/downgrade their machine if available. For the first prompt, we will use cheapest Linux server.

They will know the price and information for each server. They can even choose macOS/Windows server.

> Notice that macOS server(EC2 Mac) has a 24-hour minimum billing because of Apple license. The price information agent sees will include this.

> Notice that the computing budgets DO NOT include LLM tokens. It is by design since it will cause early stop.

### Web first

We natively supported PWA features like web push to enhance web experience and build for browser for the first day.

### Sandboxed environment

The environment used for flyco. Must be able to create and delete in minutes. For cloud providers, they should be able to change machine without erasing the disk.

We support setup scripts, however, this setup scripts will be executed after agent respond whether they satisfy with their current machine. Before they responding this, the code will also not be fetched.

By default, we allow ALL network access. In the future we will bring network control.

Each sesssion has their own environment, the environment is not shared.

## Add-on Features

### GitHub integration

We have first-class GitHub integration. Including these features:

- Behave as user(default) or flyco bot

- Auto fix CI problems
- Auto commit to the branch agent created
- Merge to X branch if user approve
- Rewrite history if user approve

Notice that these approval should NOT be handled by model itself prompt engineering. We should display our own UI for approval.

### MCP support

We support MCP and provide an central place to manage these servers.

We forbid agents to configure MCP for themselves by hook+read only.

### Skill support

We will config file as read-only and hook to forbid model to change global skills. If they wanna change global skills, they should make a MCP call to upload a zip of skill.

We will update the skills we stored in cloudflare. And ALL agents's skills directories will be updated immediately.

Notice that Claude has a different global skills directory with codex.

### Computer use

User can turn on/off computer use ability of model. We will config computer use on machine automatically, including the request of permissions on macOS.

If user turn on computer use but desktop is not installed for this Linux machine. We will install GNOME automatically.

The computer use screen will be video streaming.

### Browser use

User can turn on/off browser use ability of model.

If user turn on browser use but playwright is not installed. We will install playwright automatically. Also, if no browser is available. We will install automaitcally.

### Shared AGENTS.md

AGENTS.md is shared between agents. And they are forbidden to change their global AGENTS.md, for claude, it is CLAUDE.md.

We will config file as read-only and hook to forbid model to change global AGENTS.md/CLAUDE.md. If they wanna change AGENTS.md/CLAUDE.md, they should make a MCP call to request AGENTS.md text replace. They request will be send to user.

### .env

Agent can read-only and user can edit. We will warn user it may be lacked since we do not implemented network control yet.

### LLM usage panel

A panel to view the usage of each accounts(claude/codex)

### Cloud computing usage panel

A panel to view the usage of each accounts(azure/aws/gcp/...)

### Spot instance

We should use spot instance by default. It saves much money for user with very acceptable pause for sometimes - and we are able to resume it in minutes. Of course, we provide a toggle to turn off it.

> Tip: spot only recycle the computing part instead of storage. We will always use SSD as storage and it would not be recycled unexpectedly. However, the cost of storage will also be counted to computing budgets.

### History

We will store the session in cloudflare so that it can be resumed to ANY machine. But we do not store execution environment.

User must archive their session periodly. To encourage the archive, by default, we support up to 5 sessions. If user want to open a new session, archive previous one or change this number.

We will archive session if they are not active 1 week.

Archived sessions will still keep their turns. Only the disk of execution environment get released.

If agent want to persist their work they must commit their work. If a repo is dirty, agent will not allowed to stop unless they beyond their budgets. (here stop means agent will be kept awake instead of complete, it is not a forced commit) We will store the repo change before we releasing the disk if an automatic archive is expected. If user archive the repo manually, we will warn user and release the disk without saving the change after an approval.



## More Features

### Terminal

All unix-based terminal must be `fish` and installed `oh-my-fish`, just because i like it.

However, agent will still use bash/powershell, since they are more familar with this.

Windows terminal must use powershell. Though i don't like this.

User can interact with terminal(fish) on the web, and we support `!` command to enter command(bash) in chat. (User will be warned they should use bash syntax here)

## Cloud providers

We support following cloud providers, and we will provide a quick start with some simple questions like:

- Are you new user to these cloud providers(Azure/AWS/GCP/...)
- Are you a college student?

We will use these questions to find bonus from these cloud providers.

And here is cloud providers we currently support (i will add more in the future):

- AWS
- Google computing platform
- Azure

Also! We support user to add their own machine via SSH. (only Linux machine is supported since we require sandbox by Podman)

### Future features(not supported yet)

- Kaggle/Colab integration
- Network control

## Login

We only support GitHub login. Passkey login coming soon.

## Self-hosted

We will provide easy self-hosted powered by skyzen CLI. Self-hosted means deploying flyco to your own Cloudflare account.



## Safety

- Please utilize cloudflare to anti-bot for registeration.

## REST API

We provide REST API to interact with our backend. Also, user can create API key to use their without our frontend.

Our frontend use the same REST API with the developer-faced one. So, be tasteful when you design the API!