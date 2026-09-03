//! Everything flyco says to the agent, in one place and in compiled form.
//!
//! Two audiences read this module's output and both of them are the model:
//! the **notices** flycod injects into the conversation (the machine a
//! session starts on, the machine restarting under it) and the **tool
//! descriptions and results** its local MCP server hands back. They are the
//! same kind of thing — text the agent acts on — so they are written the
//! same way: one [askama] template per message, under
//! `crates/daemon/templates/`, with a typed context struct beside it.
//!
//! That is not decoration. These sentences are the contract: an agent that
//! is not told a resize restarts the machine will lose a build to one, and
//! an agent not told the user picked the machine will trade it away for a
//! faster one. A template renamed a field out of existence fails the build;
//! a `format!` chain would have shipped a notice with a hole in it.
//!
//! Every message that quotes a machine quotes it through
//! `machine_line.txt`, and every message that mentions the user's choice
//! includes `user_chose_machine.txt`, so the agent reads one wording for one
//! fact wherever it meets it.

use askama::Template;
use flyco_core::{BudgetStage, BudgetView, MachineOrigin, MachineState, SessionMachine};

/// One machine, reduced to the phrases a sentence about it is built from.
///
/// The numbers are formatted here rather than in the templates because
/// [`Usd`](flyco_core::Usd) already knows how to write itself as `$0.19` and
/// a template that reimplemented that would be a second opinion about
/// money. What the templates decide is the layout: which parts appear, in
/// what order, separated by what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineLine {
    /// Provider-native type name.
    pub machine_type: String,
    /// `8 vCPU / 32 GiB`, absent for a machine flyco has never measured.
    pub capacity: Option<String>,
    /// `$0.38/hr`, or what to say instead where flyco meters nothing.
    pub price: String,
    /// Whether the capacity is interruptible.
    pub spot: bool,
    /// `starts a 24-hour minimum charge of $15.60 the moment it boots`,
    /// present on exactly the license-bound types.
    pub minimum: Option<String>,
}

/// How many MiB are in a GiB. Capacities are published in MiB and read in
/// GiB.
const MIB_PER_GIB: u64 = 1024;

impl MachineLine {
    /// Reads a machine into the phrases a sentence about it needs.
    #[must_use]
    pub fn of(machine: &SessionMachine) -> Self {
        Self {
            machine_type: machine.machine_type.clone(),
            capacity: machine.capacity.as_ref().map(|capacity| {
                format!(
                    "{} vCPU / {} GiB",
                    capacity.vcpus,
                    capacity.memory_mib / MIB_PER_GIB
                )
            }),
            // Not `$0.00`: hardware the user owns is not free compute, it is
            // compute flyco does not meter, and a budget told otherwise
            // would conclude the session can run forever.
            price: machine.hourly.map_or_else(
                || "hardware the user owns".to_owned(),
                |hourly| format!("{hourly}/hr"),
            ),
            spot: machine.spot,
            minimum: machine.minimum.map(|minimum| {
                format!(
                    "starts a {}-hour minimum charge of {} the moment it boots",
                    minimum.hours, minimum.charge
                )
            }),
        }
    }
}

/// The first thing the agent reads: flyco's notice, then the user's prompt.
///
/// One message rather than two. The notice has to reach the agent *before*
/// it starts working — a machine it was not told about is one it will
/// resize away from — and a notice delivered as a message of its own would
/// open a turn answering nothing, which costs the user tokens and the
/// transcript a paragraph about a machine nobody asked about. Prepending it
/// to the first real message puts it in front of the work instead. The
/// transcript is unaffected: what browsers replay is what the room recorded
/// from the user, not what the daemon fed the harness.
#[derive(Debug, Template)]
#[template(path = "opening_message.txt", escape = "none")]
pub struct OpeningMessage {
    /// The rendered notice.
    pub notice: String,
    /// What the user actually said.
    pub text: String,
}

/// What flycod tells the agent about its machine when the session opens.
#[derive(Debug, Template)]
#[template(path = "session_start.txt", escape = "none")]
pub struct SessionStart {
    /// The machine the session opened on.
    pub line: MachineLine,
    /// Whether the user picked it rather than flyco.
    pub user_chose: bool,
}

impl SessionStart {
    /// Builds the opening notice for a machine and who chose it.
    #[must_use]
    pub fn new(machine: &SessionMachine, origin: MachineOrigin) -> Self {
        Self {
            line: MachineLine::of(machine),
            user_chose: origin == MachineOrigin::User,
        }
    }
}

/// What flycod tells the agent after its machine was replaced under it.
///
/// The restart is the part that matters and it is stated rather than
/// assumed: an agent told only "you are on a bigger machine now" would go on
/// talking to a dev server that died with the old one.
#[derive(Debug, Template)]
#[template(path = "machine_changed.txt", escape = "none")]
pub struct MachineChanged {
    /// The machine the session came back on.
    pub line: MachineLine,
    /// Whether the change restarted the machine. A resize always does.
    pub restarted: bool,
}

/// The `machine_resize` tool's description.
///
/// Rendered per session rather than fixed, because two of the things the
/// agent has to know are facts about *this* session: which types it may
/// move to at all, and whether the machine it is on was the user's choice.
#[derive(Debug, Template)]
#[template(path = "tool_machine_resize.md", escape = "none")]
pub struct ResizeDescription {
    /// The curated catalog, cheapest first.
    pub lines: Vec<MachineLine>,
    /// Whether any of them bills a minimum on boot.
    pub has_license_bound: bool,
    /// Whether the user picked the machine the session is on.
    pub user_chose: bool,
}

impl ResizeDescription {
    /// Describes the tool against a curated catalog and a machine's origin.
    #[must_use]
    pub fn new(catalog: &[SessionMachine], origin: MachineOrigin) -> Self {
        Self {
            lines: catalog.iter().map(MachineLine::of).collect(),
            has_license_bound: catalog.iter().any(SessionMachine::is_license_bound),
            user_chose: origin == MachineOrigin::User,
        }
    }
}

/// The `machine_status` tool's answer.
#[derive(Debug, Template)]
#[template(path = "machine_status.txt", escape = "none")]
pub struct MachineStatus {
    /// The machine, as a sentence is built from it.
    pub line: MachineLine,
    /// Provider-native region it runs in.
    pub region: String,
    /// Where it is in its lifecycle, in words.
    pub state: &'static str,
    /// Whether the user picked it rather than flyco.
    pub user_chose: bool,
}

impl MachineStatus {
    /// Reads the control plane's answer into the agent's.
    #[must_use]
    pub fn of(view: &flyco_core::AgentMachineView) -> Self {
        Self {
            line: MachineLine::of(&view.machine),
            region: view.region.clone(),
            state: match view.state {
                MachineState::Provisioning => "still being created",
                MachineState::Running => "running",
                MachineState::Deallocated => "stopped, with its disk kept",
                MachineState::Destroyed => "gone",
            },
            user_chose: view.origin == MachineOrigin::User,
        }
    }
}

/// The `budget_status` tool's answer.
#[derive(Debug, Template)]
#[template(path = "budget_status.txt", escape = "none")]
pub struct BudgetStatus {
    /// The limit the user set.
    pub limit: String,
    /// What machine time and storage have cost so far.
    pub spent: String,
    /// What is left.
    pub remaining: String,
    /// What that means, in the words the thresholds are named by.
    pub stage: &'static str,
}

impl BudgetStatus {
    /// Reads the control plane's accounting into the agent's.
    #[must_use]
    pub fn of(budget: &BudgetView) -> Self {
        Self {
            limit: budget.limit.to_string(),
            spent: budget.spent.to_string(),
            remaining: budget.remaining.to_string(),
            stage: match budget.stage {
                BudgetStage::Ok => "Nothing to watch yet.",
                BudgetStage::Notice50 => "Half of it is gone.",
                BudgetStage::Warn80 => "Four fifths of it are gone.",
                BudgetStage::Final90 => {
                    "Nine tenths of it are gone; the session pauses at the whole."
                }
                BudgetStage::Exhausted => {
                    "It is exhausted, and the session pauses as soon as flyco notices."
                }
            },
        }
    }
}

/// What the agent is told when the user raises an exhausted budget.
///
/// The symmetric half of the pause: the pause was enforced without a word
/// to the model — the turn was interrupted mid-thought — so lifting it has
/// to say what happened, or the agent wakes up with no account of why it
/// stopped and no instruction to continue.
#[derive(Debug, Template)]
#[template(path = "budget_raised.txt", escape = "none")]
pub struct BudgetRaised {
    /// What the session may spend now, in total.
    pub limit: String,
}

/// What `machine_resize` says when it started one.
#[derive(Debug, Template)]
#[template(path = "resize_accepted.txt", escape = "none")]
pub struct ResizeAccepted {
    /// The machine the session is moving to.
    pub line: MachineLine,
}

/// What `machine_resize` says when it raised an approval instead.
#[derive(Debug, Template)]
#[template(path = "resize_pending_approval.txt", escape = "none")]
pub struct ResizePending {
    /// The type the agent asked for.
    pub machine_type: String,
    /// What booting it costs, in the wording the user's card also quotes.
    pub minimum: String,
    /// The type the session stays on until the user decides.
    pub current_type: String,
}

/// What `machine_resize` says when the working tree is dirty.
#[derive(Debug, Template)]
#[template(path = "resize_refused_dirty.txt", escape = "none")]
pub struct ResizeRefusedDirty {
    /// `git status --short`, verbatim.
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::{
        BudgetRaised, BudgetStatus, MachineChanged, MachineLine, MachineStatus, ResizeDescription,
        ResizePending, ResizeRefusedDirty, SessionStart,
    };
    use askama::Template as _;
    use flyco_core::{
        AgentMachineView, BillingMinimum, BudgetStage, BudgetView, MachineCapacity, MachineOrigin,
        MachineState, SessionMachine, Usd,
    };

    fn linux() -> SessionMachine {
        SessionMachine {
            machine_type: "Standard_D8s_v6".to_owned(),
            hourly: Some(Usd::from_cents(38)),
            spot: true,
            capacity: Some(MachineCapacity {
                vcpus: 8,
                memory_mib: 32 * 1024,
            }),
            minimum: None,
        }
    }

    fn mac() -> SessionMachine {
        SessionMachine {
            machine_type: "mac2.metal".to_owned(),
            hourly: Some(Usd::from_cents(65)),
            spot: false,
            capacity: Some(MachineCapacity {
                vcpus: 8,
                memory_mib: 16 * 1024,
            }),
            minimum: Some(BillingMinimum::new(24, Usd::from_cents(65))),
        }
    }

    #[test]
    fn a_machine_reads_as_its_name_size_price_and_capacity_mode() {
        let line = MachineLine::of(&linux());
        assert_eq!(line.capacity.as_deref(), Some("8 vCPU / 32 GiB"));
        assert_eq!(line.price, "$0.38/hr");
        assert!(line.minimum.is_none());
    }

    #[test]
    fn hardware_the_user_owns_is_never_priced_at_zero() {
        let owned = SessionMachine {
            hourly: None,
            capacity: None,
            ..linux()
        };
        let line = MachineLine::of(&owned);
        assert_eq!(line.price, "hardware the user owns");
        assert!(!line.price.contains("0.00"));
    }

    #[test]
    fn a_license_bound_type_quotes_the_charge_in_dollars() {
        let line = MachineLine::of(&mac());
        assert_eq!(
            line.minimum.as_deref(),
            Some("starts a 24-hour minimum charge of $15.60 the moment it boots")
        );
    }

    #[test]
    fn the_opening_notice_tells_an_agent_to_leave_the_users_machine_alone() {
        let chosen = SessionStart::new(&linux(), MachineOrigin::User)
            .render()
            .expect("render");
        assert!(chosen.contains(
            "The user chose this machine themselves; do not switch it unless the task cannot \
             proceed on it, and say why when you do."
        ));

        let automatic = SessionStart::new(&linux(), MachineOrigin::Auto)
            .render()
            .expect("render");
        assert!(!automatic.contains("The user chose this machine"));
        assert!(automatic.contains("Flyco chose this machine automatically"));
    }

    #[test]
    fn the_opening_notice_states_the_machine_it_opened_on() {
        let rendered = SessionStart::new(&linux(), MachineOrigin::Auto)
            .render()
            .expect("render");
        assert!(rendered.contains("Standard_D8s_v6 · 8 vCPU / 32 GiB · $0.38/hr · spot"));
        assert!(rendered.starts_with("[flyco machine notice]"));
    }

    #[test]
    fn a_restart_notice_says_the_processes_died_and_the_disk_did_not() {
        let rendered = MachineChanged {
            line: MachineLine::of(&linux()),
            restarted: true,
        }
        .render()
        .expect("render");
        assert!(rendered.starts_with("[flyco machine notice] The machine restarted"));
        assert!(rendered.contains("Everything you had running is gone"));
        assert!(rendered.contains("The disk was kept"));

        let quiet = MachineChanged {
            line: MachineLine::of(&linux()),
            restarted: false,
        }
        .render()
        .expect("render");
        assert!(quiet.contains("Nothing was restarted"));
        assert!(!quiet.contains("Everything you had running is gone"));
    }

    #[test]
    fn the_resize_description_says_what_a_resize_costs_before_it_lists_anything() {
        let rendered = ResizeDescription::new(&[linux(), mac()], MachineOrigin::User)
            .render()
            .expect("render");

        assert!(rendered.contains("Resizing restarts the machine."));
        assert!(rendered.contains("The disk survives untouched"));
        assert!(rendered.contains("refuses while the working tree has uncommitted changes"));
        assert!(rendered.contains("`force: true`"));
        assert!(rendered.contains("- Standard_D8s_v6 · 8 vCPU / 32 GiB · $0.38/hr · spot"));
        assert!(rendered.contains(
            "- mac2.metal · 8 vCPU / 16 GiB · $0.65/hr · starts a 24-hour minimum charge of \
             $15.60 the moment it boots"
        ));
        assert!(rendered.contains("pending the user's decision"));
        assert!(rendered.contains("The user chose this machine themselves"));
    }

    #[test]
    fn a_catalog_with_no_license_bound_type_does_not_talk_about_approvals() {
        let rendered = ResizeDescription::new(&[linux()], MachineOrigin::Auto)
            .render()
            .expect("render");
        assert!(!rendered.contains("pending the user's decision"));
        assert!(!rendered.contains("The user chose this machine"));
    }

    #[test]
    fn machine_status_reports_the_live_machine_and_who_chose_it() {
        let rendered = MachineStatus::of(&AgentMachineView {
            origin: MachineOrigin::User,
            machine: mac(),
            state: MachineState::Running,
            region: "us-east-1".to_owned(),
        })
        .render()
        .expect("render");

        assert!(rendered.contains("mac2.metal"));
        assert!(rendered.contains("in us-east-1"));
        assert!(rendered.contains("the machine is running"));
        assert!(rendered.contains("Booting it already starts a 24-hour minimum charge of $15.60"));
        assert!(rendered.contains("The user chose this machine themselves"));
    }

    #[test]
    fn budget_status_says_what_is_left_and_what_the_budget_covers() {
        let rendered = BudgetStatus::of(&BudgetView {
            limit: Usd::from_dollars(10),
            spent: Usd::from_dollars(9),
            remaining: Usd::from_dollars(1),
            stage: BudgetStage::Final90,
        })
        .render()
        .expect("render");

        assert!(rendered.contains("spent $9.00 of its $10.00 compute budget, leaving $1.00"));
        assert!(rendered.contains("never for the tokens your turns cost"));
        assert!(rendered.contains("the session pauses at the whole"));
    }

    #[test]
    fn a_raised_budget_names_the_new_limit_and_tells_the_agent_to_continue() {
        let rendered = BudgetRaised {
            limit: Usd::from_dollars(25).to_string(),
        }
        .render()
        .expect("render");

        assert!(rendered.starts_with("[flyco budget notice]"));
        assert!(rendered.contains("$25.00"));
        assert!(rendered.contains("carry on from where you were interrupted"));
    }

    #[test]
    fn a_pending_approval_quotes_the_charge_the_user_is_deciding_about() {
        let rendered = ResizePending {
            machine_type: "mac2.metal".to_owned(),
            minimum: MachineLine::of(&mac()).minimum.expect("a mac bills a day"),
            current_type: "Standard_D8s_v6".to_owned(),
        }
        .render()
        .expect("render");

        assert!(rendered.starts_with("Pending the user's approval."));
        assert!(rendered.contains("$15.60"));
        assert!(rendered.contains("Standard_D8s_v6"));
    }

    #[test]
    fn a_dirty_refusal_shows_the_work_that_would_be_at_risk() {
        let rendered = ResizeRefusedDirty {
            summary: " M crates/daemon/src/mcp.rs".to_owned(),
        }
        .render()
        .expect("render");

        assert!(rendered.starts_with("Refused: the working tree has uncommitted changes."));
        assert!(rendered.contains("`force: true`"));
        assert!(rendered.contains(" M crates/daemon/src/mcp.rs"));
    }
}
