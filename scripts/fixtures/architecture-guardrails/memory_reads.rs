// Fixture for scripts/check-architecture-guardrails.py --self-test; never compiled.
// The self-test asserts the exact lines the rule flags. Lines marked BAD are flagged.
// A comment naming FROM proxiima_core.memory is skipped.
fn reads() {
    sqlx::query("SELECT id FROM proxiima_core.memory WHERE id = $1"); // BAD
    sqlx::query("SELECT 1 FROM x JOIN proxiima_core.memory m ON m.id = x.id"); // BAD
    sqlx::query("UPDATE j SET n = 1 FROM claimed, proxiima_core.memory m WHERE m.id = j.id"); // BAD
    sqlx::query("DELETE FROM x USING proxiima_core.memory m WHERE m.id = x.id"); // BAD
    sqlx::query("SELECT id FROM proxiima_core.memory"); // BAD: the lock below is another literal
    sqlx::query("SELECT 1 FOR UPDATE");
    sqlx::query(
        "SELECT id
           FROM proxiima_core.memory
          WHERE id = $1", // BAD: multi-line, no lock
    );
    sqlx::query("SELECT id FROM proxiima_core.live_memory WHERE id = $1");
    sqlx::query("SELECT id FROM proxiima_core.memory_including_forgotten WHERE id = $1");
    sqlx::query("SELECT id FROM proxiima_core.memory_head h JOIN proxiima_core.memory_version v USING (id)");
    sqlx::query("DELETE FROM proxiima_core.memory WHERE id = $1");
    sqlx::query("INSERT INTO proxiima_core.memory (id) VALUES ($1)");
    sqlx::query("UPDATE proxiima_core.memory SET kind = $1");
    sqlx::query("SELECT id FROM proxiima_core.memory WHERE id = $1 FOR UPDATE");
    sqlx::query(
        "SELECT id
           FROM proxiima_core.memory
          WHERE id = $1
          ORDER BY id
            FOR SHARE",
    );
    sqlx::query("SELECT id FROM proxiima_core.memory WHERE note = \"by\" FOR SHARE");
    sqlx::query("SELECT id FROM proxiima_core.memory WHERE id = $1 FOR NO KEY UPDATE");
    sqlx::query("SELECT id FROM proxiima_core.memory WHERE id = $1 FOR KEY SHARE");
}

#[cfg(test)]
mod tests {
    fn read_freely() {
        sqlx::query("SELECT id FROM proxiima_core.memory");
    }
}
