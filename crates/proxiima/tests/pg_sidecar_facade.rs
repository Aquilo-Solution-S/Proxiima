use proxiima::spiice::{FactPayload, PayloadKeyBuilder, PgMemoryPayload, PgMemorySidecar};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FacadeMacroFact {
    note: String,
}

impl FactPayload for FacadeMacroFact {
    const SCHEMA_ID: &'static str = "test/facade-macro-fact-v1";
    const SCHEMA_VERSION: u32 = 1;

    fn receipt_key(&self) -> Vec<u8> {
        let mut key = PayloadKeyBuilder::new(Self::SCHEMA_ID, Self::SCHEMA_VERSION);
        key.field_str("note", &self.note);
        key.finish()
    }

    fn render(&self) -> String {
        self.note.clone()
    }

    fn sidecar_table() -> Option<&'static str> {
        Some("public.facade_macro_fact_v1")
    }
}

impl proxiima::spiice::PayloadProjection for FacadeMacroFact {
    const KEY_FIELDS: &'static [proxiima::spiice::KeyField<Self>] = &[];
    fn display(&self) -> proxiima::spiice::PayloadDisplay {
        proxiima::spiice::PayloadDisplay {
            body: Some(self.note.clone()),
            ..Default::default()
        }
    }
    fn durable(&self) -> Result<proxiima::spiice::DurablePayload, String> {
        let mut builder = proxiima::spiice::DurablePayloadBuilder::new();
        builder.field("note", &self.note)?;
        Ok(builder.finish())
    }
}

proxiima::spiice::pg_sidecar! {
    payload: FacadeMacroFact,
    row: FacadeMacroFactRow,
    kinds: [Fact],
    table: "public.facade_macro_fact_v1",
    key: memory_id,
    fields: {
        note => note: (text),
    },
}

fn assert_pg_sidecar_traits<T: FactPayload + PgMemorySidecar + PgMemoryPayload>() {}

#[test]
fn pg_sidecar_macro_is_reachable_from_facade() {
    assert_pg_sidecar_traits::<FacadeMacroFact>();
    let payload = FacadeMacroFact {
        note: "facade macro ok".to_string(),
    };
    assert_eq!(payload.render(), "facade macro ok");
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FacadeFollowsFact {
    mark: String,
}

impl FactPayload for FacadeFollowsFact {
    const SCHEMA_ID: &'static str = "test/facade-follows-fact-v1";
    const SCHEMA_VERSION: u32 = 1;

    fn receipt_key(&self) -> Vec<u8> {
        let mut key = PayloadKeyBuilder::new(Self::SCHEMA_ID, Self::SCHEMA_VERSION);
        key.field_str("mark", &self.mark);
        key.finish()
    }

    fn render(&self) -> String {
        self.mark.clone()
    }

    fn sidecar_table() -> Option<&'static str> {
        Some("public.facade_follows_fact_v1")
    }
}

impl proxiima::spiice::PayloadProjection for FacadeFollowsFact {
    const KEY_FIELDS: &'static [proxiima::spiice::KeyField<Self>] = &[];
    fn display(&self) -> proxiima::spiice::PayloadDisplay {
        proxiima::spiice::PayloadDisplay {
            body: Some(self.mark.clone()),
            ..Default::default()
        }
    }
    fn durable(&self) -> Result<proxiima::spiice::DurablePayload, String> {
        let mut builder = proxiima::spiice::DurablePayloadBuilder::new();
        builder.field("mark", &self.mark)?;
        Ok(builder.finish())
    }
}

proxiima::spiice::pg_sidecar! {
    payload: FacadeFollowsFact,
    row: FacadeFollowsFactRow,
    kinds: [Fact],
    table: "public.facade_follows_fact_v1",
    key: t,
    owner: follows,
    fields: {
        mark => mark: (text),
    },
}

#[test]
fn pg_sidecar_owner_follows_token_is_accepted() {
    use proxiima::spiice::SidecarOwnerMode;
    assert_pg_sidecar_traits::<FacadeFollowsFact>();
    assert_eq!(
        <FacadeFollowsFact as PgMemoryPayload>::OWNER,
        SidecarOwnerMode::Follows
    );
    assert!(<FacadeFollowsFact as PgMemoryPayload>::OWNER.stamps_owner_at_insert());
    assert!(!<FacadeFollowsFact as PgMemoryPayload>::OWNER.is_pinned());
}

/// A payload whose insert is written by hand writes its row through the
/// SDK's insert, which stamps the owner as `OWNER` declares.
struct FacadeEnvelope {
    envelope: serde_json::Value,
}

impl PgMemorySidecar for FacadeEnvelope {
    fn insert_memory_sidecar<'t>(
        &'t self,
        tx: &'t mut sqlx::Transaction<'_, sqlx::Postgres>,
        memory_id: proxiima::spiice::MemoryId,
        permit: proxiima::spiice::SidecarInsertPermit,
    ) -> proxiima::spiice::PgSidecarFuture<'t> {
        Box::pin(proxiima::spiice::insert_memory_sidecar_row::<Self>(
            tx,
            memory_id,
            permit,
            "public.facade_envelope_v1",
            &[("envelope", None)],
            |query| Ok(query.bind(&self.envelope)),
        ))
    }
}

impl PgMemoryPayload for FacadeEnvelope {
    const OWNER: proxiima::spiice::SidecarOwnerMode = proxiima::spiice::SidecarOwnerMode::Follows;
    const MEMORY_KEY_COLUMN: &'static str = "t";
}

fn assert_hand_written_sidecar<T: PgMemorySidecar + PgMemoryPayload>() {}

#[test]
fn a_hand_written_sidecar_inserts_through_the_facade() {
    assert_hand_written_sidecar::<FacadeEnvelope>();
}
