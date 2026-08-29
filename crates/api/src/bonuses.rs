//! The free-credit programmes the quickstart questionnaire matches against.
//!
//! Two questions decide it, so the table is keyed on the same two facts:
//! whether the user is new to the provider, and whether they are a student.
//!
//! # Where the numbers come from
//!
//! Every amount here is one the provider states on the linked page. Where a
//! programme's headline credit is not a fixed published figure — AWS
//! restructured its free tier and the credit now depends on what the account
//! does after sign-up — the entry describes the programme and carries **no**
//! amount. An invented number would be worse than none: a user plans a
//! session budget against it.

use flyco_core::{CloudProviderKind, ProviderBonusHint, QuickstartAnswers, Usd};

/// One programme, and who it is for.
struct Programme {
    provider: CloudProviderKind,
    title: &'static str,
    detail: &'static str,
    /// Headline credit, when the provider publishes a fixed figure.
    credit: Option<Usd>,
    url: &'static str,
    /// Only offered to somebody who has never held an account here.
    new_customers_only: bool,
    /// Only offered to students.
    students_only: bool,
}

/// Every programme flyco knows about.
const PROGRAMMES: &[Programme] = &[
    Programme {
        provider: CloudProviderKind::Azure,
        title: "Azure free account",
        detail: "New Azure accounts start with credit to spend in the first 30 days, \
                 alongside a set of services that stay free for 12 months. A card is \
                 required for identity verification and nothing is charged until you \
                 move to pay-as-you-go.",
        credit: Some(Usd::from_dollars(200)),
        url: "https://azure.microsoft.com/free/",
        new_customers_only: true,
        students_only: false,
    },
    Programme {
        provider: CloudProviderKind::Azure,
        title: "Azure for Students",
        detail: "Credit for a year with no credit card, renewable while you remain \
                 enrolled. Note that it is not one of the offer types Azure supports \
                 spot capacity on in every region, so flyco falls back to on-demand \
                 where spot is refused.",
        credit: Some(Usd::from_dollars(100)),
        url: "https://azure.microsoft.com/free/students/",
        new_customers_only: false,
        students_only: true,
    },
    Programme {
        provider: CloudProviderKind::Gcp,
        title: "Google Cloud free trial",
        detail: "New Google Cloud accounts get trial credit valid for 90 days, on top \
                 of the Free tier's always-free allowances.",
        credit: Some(Usd::from_dollars(300)),
        url: "https://cloud.google.com/free",
        new_customers_only: true,
        students_only: false,
    },
    Programme {
        provider: CloudProviderKind::Aws,
        title: "AWS Free Tier",
        detail: "New AWS accounts get free-tier allowances and sign-up credits. The \
                 credit is not a single published figure — it depends on the plan you \
                 choose and on what the account does afterwards — so check the page \
                 for what applies to you today.",
        credit: None,
        url: "https://aws.amazon.com/free/",
        new_customers_only: true,
        students_only: false,
    },
    Programme {
        provider: CloudProviderKind::Aws,
        title: "AWS Educate",
        detail: "Free training and hands-on labs for students, with no card and no \
                 AWS account required. It funds learning rather than arbitrary \
                 workloads, so it will not usually pay for a flyco session — it is \
                 listed because it is the AWS programme students actually qualify for.",
        credit: None,
        url: "https://aws.amazon.com/education/awseducate/",
        new_customers_only: false,
        students_only: true,
    },
    Programme {
        provider: CloudProviderKind::Azure,
        title: "GitHub Student Developer Pack",
        detail: "Bundles the student offers of several providers, including Azure for \
                 Students, behind one verification. Worth claiming first, because it \
                 is the same verification the others ask for.",
        credit: None,
        url: "https://education.github.com/pack",
        new_customers_only: false,
        students_only: true,
    },
];

impl Programme {
    /// Whether these answers qualify.
    const fn matches(&self, answers: QuickstartAnswers) -> bool {
        (!self.new_customers_only || answers.new_to_provider)
            && (!self.students_only || answers.is_student)
    }

    fn hint(&self) -> ProviderBonusHint {
        ProviderBonusHint {
            provider: self.provider,
            title: self.title.to_owned(),
            detail: self.detail.to_owned(),
            credit: self.credit,
            url: self.url.to_owned(),
        }
    }
}

/// The programmes these answers qualify for, in the order they are listed.
#[must_use]
pub fn matching(answers: &QuickstartAnswers) -> Vec<ProviderBonusHint> {
    PROGRAMMES
        .iter()
        .filter(|programme| programme.matches(*answers))
        .map(Programme::hint)
        .collect()
}

#[cfg(test)]
mod tests {
    use flyco_core::{CloudProviderKind, QuickstartAnswers};

    use super::{PROGRAMMES, matching};

    fn answers(new_to_provider: bool, is_student: bool) -> QuickstartAnswers {
        QuickstartAnswers {
            new_to_provider,
            is_student,
        }
    }

    #[test]
    fn a_returning_non_student_is_told_about_nothing_they_cannot_claim() {
        assert!(
            matching(&answers(false, false)).is_empty(),
            "every programme flyco knows is either new-customer or student"
        );
    }

    #[test]
    fn a_new_customer_sees_the_sign_up_credits() {
        let hints = matching(&answers(true, false));
        assert!(
            hints
                .iter()
                .any(|hint| hint.provider == CloudProviderKind::Azure)
        );
        assert!(
            hints
                .iter()
                .any(|hint| hint.provider == CloudProviderKind::Gcp)
        );
        assert!(
            hints.iter().all(|hint| !hint.title.contains("Student")),
            "a non-student is not shown student programmes"
        );
    }

    #[test]
    fn a_new_student_sees_both_kinds() {
        let hints = matching(&answers(true, true));
        assert!(hints.iter().any(|hint| hint.title == "Azure for Students"));
        assert!(hints.iter().any(|hint| hint.title == "Azure free account"));
    }

    #[test]
    fn a_returning_student_still_qualifies_for_the_student_tiers() {
        let hints = matching(&answers(false, true));
        assert!(!hints.is_empty(), "a new student should be offered something");
        assert!(
            hints
                .iter()
                .all(|hint| hint.title.contains("Student") || hint.title.contains("Educate")),
            "a returning customer is not shown new-customer credit"
        );
    }

    #[test]
    fn every_programme_links_to_the_provider_that_runs_it() {
        for programme in PROGRAMMES {
            let url = programme.url;
            assert!(
                url.starts_with("https://"),
                "`{}` must link somewhere real",
                programme.title
            );
            assert!(
                !url.contains("example."),
                "`{}` must link to the real programme",
                programme.title
            );
        }
    }

    #[test]
    fn a_programme_without_a_published_figure_states_no_amount() {
        // Inventing one would be worse than none: a user plans a session
        // budget against it.
        let aws = matching(&answers(true, false))
            .into_iter()
            .find(|hint| hint.title == "AWS Free Tier")
            .expect("AWS is offered to new customers");
        assert_eq!(aws.credit, None);
        assert!(aws.detail.contains("not a single published figure"));
    }
}
