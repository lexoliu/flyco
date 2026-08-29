//! Proving an access key works, and nothing else.
//!
//! `sts:GetCallerIdentity` is the cheapest call AWS offers that exercises a
//! whole credential: it needs no permission at all — it cannot be denied by
//! an IAM policy — costs nothing, creates nothing, and answers with the
//! account the key belongs to. That makes it the right check at the moment
//! an account is linked, which is where a bad credential is cheap to fix,
//! rather than at the first provision, where it strands a half-created
//! session.
//!
//! It proves the key is real. It does not prove the key may start an
//! instance: the permissions a policy grants are only knowable by trying,
//! and a link-time simulation of them would be a second, weaker opinion
//! about a question the first provision answers exactly.

use serde::Deserialize;

/// Signing name of the Security Token Service.
pub const SERVICE: &str = "sts";

/// The API version this call pins.
pub const API_VERSION: &str = "2011-06-15";

/// The region the identity check is signed against.
///
/// STS has a global endpoint and a regional one in every region; the check
/// uses `us-east-1`, which every account has enabled — a regional endpoint
/// in a region the account never opted into would refuse a perfectly good
/// key.
pub const REGION: &str = "us-east-1";

/// Endpoint of the Security Token Service.
pub const ENDPOINT: &str = "https://sts.us-east-1.amazonaws.com/";

/// `GetCallerIdentity` takes no parameters at all.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct GetCallerIdentity {}

/// What `GetCallerIdentity` answers.
#[derive(Debug, Clone, Deserialize)]
pub struct GetCallerIdentityResponse {
    /// The result.
    #[serde(rename = "GetCallerIdentityResult")]
    pub result: CallerIdentity,
}

/// Who the caller is.
#[derive(Debug, Clone, Deserialize)]
pub struct CallerIdentity {
    /// The twelve-digit account id.
    #[serde(rename = "Account", default)]
    pub account: String,
    /// The ARN of the principal the key belongs to.
    #[serde(rename = "Arn", default)]
    pub arn: String,
}

#[cfg(test)]
mod tests {
    use super::GetCallerIdentityResponse;
    use crate::aws::ec2::decode;
    use crate::http::HttpResponse;

    #[test]
    fn an_identity_names_the_account_the_key_belongs_to() {
        let answer: GetCallerIdentityResponse = decode(&HttpResponse::new(
            200,
            include_bytes!("../../fixtures/aws/get_caller_identity.xml").to_vec(),
        ))
        .expect("the identity fixture parses");

        assert_eq!(answer.result.account, "123456789012");
        assert!(answer.result.arn.ends_with(":user/flyco"));
    }
}
