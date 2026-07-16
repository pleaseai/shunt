use crate::auth::inbound::{constant_time_eq, parse_tokens};

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Identity {
    pub sub: String,
    pub email: String,
    pub name: String,
}

pub trait ApprovalProvider: Send + Sync {
    fn verify(&self, login: &str, secret: &str) -> Option<Identity>;
}

#[derive(Clone)]
pub struct StaticUsers {
    users: Vec<(String, String)>,
}

impl StaticUsers {
    pub fn parse(raw: &str) -> Result<Self, String> {
        parse_tokens(raw).map(|users| Self { users })
    }
}

impl ApprovalProvider for StaticUsers {
    fn verify(&self, login: &str, secret: &str) -> Option<Identity> {
        let mut matched = None;
        for (email, expected) in &self.users {
            let secret_matches = constant_time_eq(secret.as_bytes(), expected.as_bytes());
            if email == login && secret_matches {
                matched = Some(Identity {
                    sub: email.clone(),
                    email: email.clone(),
                    name: email.split('@').next().unwrap_or(email).to_string(),
                });
            }
        }
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::{ApprovalProvider, StaticUsers};

    #[test]
    fn parses_users_and_builds_identity() {
        let users = StaticUsers::parse("dev@example.com:password, ops@example.com:secret")
            .expect("valid users");

        let identity = users
            .verify("dev@example.com", "password")
            .expect("matching user");
        assert_eq!(identity.sub, "dev@example.com");
        assert_eq!(identity.email, "dev@example.com");
        assert_eq!(identity.name, "dev");
        assert!(users.verify("dev@example.com", "wrong").is_none());
        assert!(users.verify("missing@example.com", "password").is_none());
    }

    #[test]
    fn rejects_malformed_and_duplicate_users() {
        assert!(StaticUsers::parse("").is_err());
        assert!(StaticUsers::parse("dev@example.com").is_err());
        assert!(StaticUsers::parse("dev@example.com:").is_err());
        assert!(StaticUsers::parse("dev@example.com:a,dev@example.com:b").is_err());
    }
}
