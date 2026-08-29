//! Which AMI a machine boots.
//!
//! An AMI id is both region- and architecture-specific, so there is no such
//! thing as "the Ubuntu 24.04 image" as a constant — the same release has a
//! different id in every region, and a new id every time Canonical
//! republishes it. Hardcoding one would pin every flyco machine to a stale
//! image in one region.
//!
//! Canonical publishes the current id as a **public SSM parameter** in every
//! region, which is the vendor's own answer to the question and the one
//! their own documentation points at. Flyco reads it through
//! `ssm.{region}.amazonaws.com`, with the architecture substituted from the
//! instance type's own answer — never from its name, because `t4g` is Arm
//! and `t3` is x86 and pairing either with the other's image fails at
//! launch.
//!
//! The alternative, `DescribeImages` filtered by Canonical's owner id and a
//! name pattern, returns hundreds of rows that have to be sorted by creation
//! date, and answers "the newest thing matching a string I made up" rather
//! than "the current image".

use serde::{Deserialize, Serialize};

use crate::ProviderError;

/// Signing name of the Systems Manager API.
pub const SERVICE: &str = "ssm";

/// The JSON-RPC target that reads one parameter.
pub const TARGET: &str = "AmazonSSM.GetParameter";

/// Body of `GetParameter`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct GetParameter {
    /// The parameter's name.
    pub name: String,
    /// Public parameters are never encrypted.
    pub with_decryption: bool,
}

/// What `GetParameter` answers.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct GetParameterResponse {
    /// The parameter.
    pub parameter: Parameter,
}

/// One parameter.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Parameter {
    /// Its value — here, an `ami-…` id.
    #[serde(default)]
    pub value: String,
}

/// The instruction set an AMI is published for, in Canonical's spelling.
///
/// EC2 names an architecture `x86_64`/`arm64` and Canonical's parameter path
/// names it `amd64`/`arm64`, so the translation is explicit rather than a
/// substring that happens to line up.
#[must_use]
pub fn image_architecture(ec2_architecture: &str) -> Option<&'static str> {
    match ec2_architecture {
        "x86_64" => Some("amd64"),
        "arm64" => Some("arm64"),
        _ => None,
    }
}

/// The public SSM parameter naming the current Ubuntu 24.04 LTS image.
///
/// The path is Canonical's own layout: release, channel, architecture,
/// virtualization type, root device. `current` is what makes it track
/// republications instead of pinning one build.
#[must_use]
pub fn parameter_name(image_architecture: &str) -> String {
    format!(
        "/aws/service/canonical/ubuntu/server/24.04/stable/current/\
         {image_architecture}/hvm/ebs-gp3/ami-id"
    )
}

/// The parameter that names the image for one EC2 architecture.
///
/// # Errors
///
/// Returns [`ProviderError::Malformed`] when the instance type runs an
/// instruction set flyco publishes no image for — which is a machine that
/// must not be provisioned rather than one to guess an image for.
pub fn parameter_for(ec2_architecture: &str) -> Result<GetParameter, ProviderError> {
    let architecture = image_architecture(ec2_architecture).ok_or(ProviderError::Malformed(
        "this instance type runs an instruction set flyco publishes no image for",
    ))?;
    Ok(GetParameter {
        name: parameter_name(architecture),
        with_decryption: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{GetParameterResponse, image_architecture, parameter_for, parameter_name};

    #[test]
    fn the_parameter_path_is_canonicals_own_layout() {
        assert_eq!(
            parameter_name("arm64"),
            "/aws/service/canonical/ubuntu/server/24.04/stable/current/arm64/hvm/ebs-gp3/ami-id"
        );
    }

    #[test]
    fn ec2s_architecture_names_are_translated_to_canonicals() {
        assert_eq!(image_architecture("x86_64"), Some("amd64"));
        assert_eq!(image_architecture("arm64"), Some("arm64"));
        // A 32-bit or Mac instruction set has no flyco image, and guessing
        // one would produce a machine that fails at launch.
        assert_eq!(image_architecture("i386"), None);
    }

    #[test]
    fn an_instruction_set_with_no_image_is_refused_rather_than_guessed() {
        assert!(
            parameter_for("x86_64")
                .expect("an x86 parameter")
                .name
                .contains("/amd64/")
        );
        parameter_for("arm64_mac").expect_err("flyco publishes no macOS image");
    }

    #[test]
    fn the_parameters_value_is_the_image_id() {
        let answer: GetParameterResponse =
            serde_json::from_str(include_str!("../../fixtures/aws/get_parameter.json"))
                .expect("the parameter fixture parses");
        assert_eq!(answer.parameter.value, "ami-0c2b8ca1dad447f8a");
    }
}
