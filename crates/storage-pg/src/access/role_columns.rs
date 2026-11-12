//! The five columns every stored role has, and the one codec between them and
//! [`Role`]: the group role projection, Goal access grants and delegated
//! authority grants all hold `read_ceiling`, `write_ceiling`, `goal_access`,
//! `administers` and `manage` (a delegated grant selects `false AS manage`).

use proxiima_core::{AccessCeiling, GoalAccess, Role, StorageError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct RoleColumns {
    pub(crate) read_ceiling: AccessCeiling,
    pub(crate) write_ceiling: AccessCeiling,
    pub(crate) goal_access: GoalAccess,
    pub(crate) administers: bool,
    pub(crate) manage: bool,
}

impl RoleColumns {
    /// Every role has columns.
    pub(crate) const fn of(role: Role) -> Self {
        Self {
            read_ceiling: role.read_ceiling(),
            write_ceiling: role.write_ceiling(),
            goal_access: role.goals(),
            administers: role.administers(),
            manage: role.manages(),
        }
    }

    /// The role these columns store.
    ///
    /// # Errors
    ///
    /// Returns `ConstraintViolation` when the write ceiling is above the read
    /// ceiling; the tables refuse such a row, so only a damaged store has one.
    pub(crate) fn role(self) -> Result<Role, StorageError> {
        let role = Role::new(self.read_ceiling, self.write_ceiling)
            .map_err(|error| {
                StorageError::ConstraintViolation(format!("stored role is invalid: {error}"))
            })?
            .with_goals(self.goal_access);
        let role = if self.administers {
            role.administering()
        } else {
            role
        };
        Ok(if self.manage { role.managing() } else { role })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CEILINGS: [AccessCeiling; 4] = [
        AccessCeiling::None,
        AccessCeiling::Fact,
        AccessCeiling::Abstraction,
        AccessCeiling::Perspective,
    ];
    const GOALS: [GoalAccess; 3] = [GoalAccess::None, GoalAccess::Reads, GoalAccess::Works];

    /// Every role the type admits round-trips, including the ones the old
    /// four-rung form could not hold: Goal work under a Fact ceiling, reading
    /// Goals without working them, and administration under a Fact write limit.
    #[test]
    fn every_role_round_trips_through_its_columns() {
        for read in CEILINGS {
            for write in CEILINGS.into_iter().filter(|write| *write <= read) {
                for goals in GOALS {
                    for (administers, manage) in
                        [(false, false), (true, false), (false, true), (true, true)]
                    {
                        let role = Role::new(read, write).expect("write within read");
                        let role = role.with_goals(goals);
                        let role = if administers {
                            role.administering()
                        } else {
                            role
                        };
                        let role = if manage { role.managing() } else { role };
                        assert_eq!(RoleColumns::of(role).role().expect("stored role"), role);
                    }
                }
            }
        }
    }

    #[test]
    fn a_write_ceiling_above_the_read_ceiling_is_refused() {
        let columns = RoleColumns {
            write_ceiling: AccessCeiling::Abstraction,
            ..RoleColumns::of(Role::ingest())
        };
        assert!(matches!(
            columns.role(),
            Err(StorageError::ConstraintViolation(_))
        ));
    }
}
