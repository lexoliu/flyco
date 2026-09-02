//! Reading a family and a generation out of a provider's own type naming.
//!
//! Two of the three clouds publish no field for either. EC2 and Compute
//! Engine both encode them in the type name itself, and both use the same
//! grammar to do it: letters naming the family, digits numbering the
//! generation, then more letters qualifying the silicon —
//! `m7g`, `c4a`, `t2a`, `mac2`, `x2iedn`. That grammar is written once here
//! rather than twice in two drivers, and never in the frontend: what reaches
//! a browser is a
//! [`MachineLineage`](flyco_core::machine::MachineLineage), already parsed.
//!
//! The qualifier stays *in* the family key rather than being dropped,
//! because it is what makes two types different machines rather than two
//! generations of one: `m7g` is Graviton and `m7i` is Intel, and calling the
//! newer of the two a supersession of the other would hide an architecture a
//! user asked for. `m6g` and `m7g` share the key `mg` and differ only in
//! generation, which is exactly the pair curation is meant to collapse.
//!
//! Azure is not parsed here at all: it publishes the family as a field
//! (the quota family, e.g. `standardDSv6Family`), and reading the field is
//! always better than reading a name — see [`crate::azure::skus`].

/// A type name's family key and generation, as its provider spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Series {
    /// Family key: the letters before the generation, plus the qualifier
    /// after it. Stable across generations of the same machine.
    pub family: String,
    /// The generation, when the name carries one.
    ///
    /// Absent from names that begin with letters and then do something other
    /// than count — `u-6tb1` — which makes such a name a family of one
    /// rather than an old generation of something else.
    pub generation: Option<u32>,
}

/// Splits `<letters><digits><qualifier>` into a family and a generation.
///
/// Returns nothing for a name that does not begin with a letter, which is
/// not a shape any provider's type names take and therefore a name this
/// cannot honestly say anything about.
#[must_use]
pub fn series(name: &str) -> Option<Series> {
    let letters: String = name.chars().take_while(char::is_ascii_alphabetic).collect();
    if letters.is_empty() {
        return None;
    }

    let rest = &name[letters.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let qualifier = &rest[digits.len()..];

    Some(Series {
        family: format!("{letters}{qualifier}"),
        generation: digits.parse().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Series, series};

    fn parsed(name: &str) -> (String, Option<u32>) {
        let Series { family, generation } = series(name).expect("the name parses");
        (family, generation)
    }

    #[test]
    fn a_generation_is_the_digits_between_the_letters() {
        assert_eq!(parsed("m7g"), ("mg".to_owned(), Some(7)));
        assert_eq!(parsed("m6g"), ("mg".to_owned(), Some(6)));
        assert_eq!(parsed("t3"), ("t".to_owned(), Some(3)));
        assert_eq!(parsed("c4a"), ("ca".to_owned(), Some(4)));
        assert_eq!(parsed("e2"), ("e".to_owned(), Some(2)));
    }

    #[test]
    fn the_qualifier_stays_in_the_family_because_it_names_the_silicon() {
        // Intel, AMD and Graviton generations of `m` are three families, not
        // one with three generations: `m7i` does not supersede `m6g`.
        assert_eq!(parsed("m7i").0, "mi");
        assert_eq!(parsed("m7a").0, "ma");
        assert_eq!(parsed("m7g").0, "mg");
        assert_eq!(parsed("m6g").0, "mg");
    }

    #[test]
    fn a_multi_letter_qualifier_survives_whole() {
        assert_eq!(parsed("x2iedn"), ("xiedn".to_owned(), Some(2)));
        assert_eq!(parsed("m7i-flex"), ("mi-flex".to_owned(), Some(7)));
        assert_eq!(parsed("mac2"), ("mac".to_owned(), Some(2)));
    }

    #[test]
    fn a_name_that_does_not_count_is_a_family_of_one() {
        assert_eq!(parsed("u-6tb1"), ("u-6tb1".to_owned(), None));
    }

    #[test]
    fn a_name_that_is_not_a_type_name_parses_to_nothing() {
        assert_eq!(series(""), None);
        assert_eq!(series("7g"), None);
    }
}
