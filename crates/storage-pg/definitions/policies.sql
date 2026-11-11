-- Core row-level security policies. The installer replaces only the three policies declared for each table.
-- Every table is installed once through owner_rls_apply_class; exceptional expressions follow each call.

-- Generate SQL for an unaliased, schema-qualified target relation.
-- Parent queries retain their ordinary RLS; generation does not bind a scope.
CREATE OR REPLACE FUNCTION proxiima_core.owner_rls_class_predicates(caller text, class_label text, target_schema text, target_table text, class proxiima_core.owner_rls_class, owner_column text)
 RETURNS TABLE(read_expr text, write_expr text, select_expr text)
 LANGUAGE plpgsql
 SET search_path TO 'pg_catalog'
AS $function$
DECLARE
    rel_oid oid;
    fk jsonb;
    fk_child text;
    fk_schema text;
    fk_table text;
    fk_column text;
    parent_has_t boolean;
    memory_key text;
    memory_keys integer;
    memory_on_key boolean;
    direction text;
    fact_owners text;
    abstraction_owners text;
    perspective_owners text;
    goal_owners text;
    kind_of text;
BEGIN
    SELECT c.oid INTO rel_oid
      FROM pg_class AS c JOIN pg_namespace AS n ON n.oid = c.relnamespace
     WHERE n.nspname = target_schema AND c.relname = target_table AND c.relkind IN ('r', 'p');

    CASE class
        WHEN 'owner_id' THEN
            memory_key := CASE WHEN target_schema = 'proxiima_core' THEN
                CASE target_table
                    WHEN 'projection' THEN 'memory_id'
                    WHEN 'embeddings' THEN 'entity_id'
                    WHEN 'embedding_heads' THEN 'entity_id'
                    WHEN 'embedding_jobs' THEN 'entity_id'
                    WHEN 'vector_index_debt' THEN 'entity_id'
                END
            END;
            IF memory_key IS NOT NULL THEN
                IF owner_column IS DISTINCT FROM 'owner_id' THEN
                    RAISE EXCEPTION '%: %.% requires owner column owner_id',
                        caller, target_schema, target_table
                        USING ERRCODE = 'PX002';
                END IF;
                FOREACH direction IN ARRAY ARRAY['read', 'write'] LOOP
                    fact_owners := format('(SELECT COALESCE(NULLIF(current_setting(%L, true), %L)::uuid[], %L::uuid[]))::uuid[]',
                        CASE direction WHEN 'read' THEN 'app.owner' ELSE 'app.write_owner' END, '', '{}');
                    abstraction_owners := format('(SELECT COALESCE(NULLIF(current_setting(%L, true), %L)::uuid[], %L::uuid[]))::uuid[]',
                        'app.' || direction || '_abstraction', '', '{}');
                    perspective_owners := format('(SELECT COALESCE(NULLIF(current_setting(%L, true), %L)::uuid[], %L::uuid[]))::uuid[]',
                        'app.' || direction || '_perspective', '', '{}');
                    goal_owners := format('(SELECT COALESCE(NULLIF(current_setting(%L, true), %L)::uuid[], %L::uuid[]))::uuid[]',
                        'app.' || direction || '_goal', '', '{}');
                    kind_of := format('CASE parent.kind::text WHEN ''fact'' THEN %s WHEN ''abstraction'' THEN %s WHEN ''perspective'' THEN %s ELSE %s END',
                        fact_owners, abstraction_owners, perspective_owners, goal_owners);
                    kind_of := format($expr$(
                        %1$I.%2$I.%3$I = ANY(%4$s)
                        OR (
                            %1$I.%2$I.%3$I = ANY(%5$s)
                            AND EXISTS (
                                SELECT 1 FROM proxiima_core.memory AS parent
                                 WHERE parent.t = %1$I.%2$I.%6$I
                                   AND parent.owner_id = ANY(%7$s)
                            )
                        )
                    )$expr$, target_schema, target_table, owner_column,
                        goal_owners, fact_owners, memory_key, kind_of);
                    IF direction = 'read' THEN
                        read_expr := kind_of;
                    ELSE
                        write_expr := kind_of;
                    END IF;
                END LOOP;
            ELSE
                read_expr := format($expr$%I.%I.%I = ANY((SELECT COALESCE(NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[]))::uuid[])$expr$, target_schema, target_table, owner_column);
                write_expr := format($expr$%I.%I.%I = ANY((SELECT COALESCE(NULLIF(current_setting('app.write_owner', true), '')::uuid[], '{}'::uuid[]))::uuid[])$expr$, target_schema, target_table, owner_column);
            END IF;
            IF memory_key IS NULL AND EXISTS (
                SELECT 1 FROM pg_constraint c WHERE c.conrelid=rel_oid AND c.contype='f'
                AND c.confrelid='proxiima_core.memory'::regclass AND array_length(c.conkey,1)=1
            ) THEN
                memory_key := proxiima_core.owner_rls_memory_key(caller,target_schema,target_table);
            END IF;
            IF memory_key IS NOT NULL THEN
                read_expr := format('(%s) OR EXISTS (SELECT 1 FROM proxiima_core.memory parent WHERE parent.t=%I.%I.%I AND parent.owner_id=%I.%I.%I AND proxiima_core.goal_memory_read_admitted(parent.t))',
                    read_expr,target_schema,target_table,memory_key,target_schema,target_table,owner_column);
            END IF;
        WHEN 'memory_owner' THEN
            memory_key := proxiima_core.owner_rls_memory_key(caller,target_schema,target_table);
            IF NOT EXISTS (
                SELECT 1 FROM pg_attribute
                 WHERE attrelid = rel_oid AND attname = memory_key AND atttypid = 'uuid'::regtype
                   AND attnum > 0 AND NOT attisdropped
            ) THEN
                RAISE EXCEPTION '%: %.% is % but has no uuid t column',
                    caller, target_schema, target_table, class_label
                    USING ERRCODE = 'PX002';
            END IF;
            -- Reading this sidecar follows the parent memory's complete RLS
            -- admission, including exact Goal routes. Do not mirror its read
            -- policy in a second owner/Goal expression here. Writes still
            -- require an owner write ceiling for the parent's actual kind.
            read_expr := format('EXISTS (SELECT 1 FROM proxiima_core.memory AS parent WHERE parent.t = %I.%I.%I)',
                target_schema, target_table, memory_key);
            kind_of := format('CASE parent.kind::text WHEN ''fact'' THEN %s WHEN ''abstraction'' THEN %s ELSE %s END',
                '(SELECT COALESCE(NULLIF(current_setting(''app.write_owner'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[]',
                '(SELECT COALESCE(NULLIF(current_setting(''app.write_abstraction'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[]',
                '(SELECT COALESCE(NULLIF(current_setting(''app.write_perspective'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[]');
            write_expr := format('EXISTS (SELECT 1 FROM proxiima_core.memory AS parent WHERE parent.t = %I.%I.%I AND parent.owner_id = ANY(%s))',
                target_schema, target_table, memory_key, kind_of);
        WHEN 'ownerless' THEN
            read_expr := 'false';
            write_expr := 'false';
        WHEN 'fk_parent' THEN
            fk := proxiima_core.owner_rls_parent_fk(caller, target_schema, target_table);
            fk_child := fk ->> 'child_column';
            fk_schema := fk ->> 'parent_schema';
            fk_table := fk ->> 'parent_table';
            fk_column := fk ->> 'parent_column';
            SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = format('%I.%I', fk_schema, fk_table)::regclass AND attname = 't' AND attnum > 0 AND NOT attisdropped) INTO parent_has_t;
            read_expr := format('EXISTS (SELECT 1 FROM %I.%I AS parent WHERE parent.%I = %I.%I.%I AND current_setting(''app.proxiima_scope'', true) = ''owner'')', fk_schema, fk_table, fk_column, target_schema, target_table, fk_child);
            IF fk_schema = 'proxiima_core' AND fk_table = 'memory' THEN
                write_expr := format('EXISTS (SELECT 1 FROM proxiima_core.memory AS parent WHERE parent.%I = %I.%I.%I AND ((parent.kind = ''fact'' AND parent.owner_id = ANY((SELECT COALESCE(NULLIF(current_setting(''app.write_owner'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[])) OR (parent.kind = ''abstraction'' AND parent.owner_id = ANY((SELECT COALESCE(NULLIF(current_setting(''app.write_abstraction'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[])) OR (parent.kind = ''perspective'' AND parent.owner_id = ANY((SELECT COALESCE(NULLIF(current_setting(''app.write_perspective'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[]))))', fk_column, target_schema, target_table, fk_child);
            ELSIF fk_schema = 'proxiima_core' AND fk_table = 'goal' THEN
                write_expr := format('EXISTS (SELECT 1 FROM proxiima_core.goal AS parent WHERE parent.%I = %I.%I.%I AND parent.owner_id = ANY((SELECT COALESCE(NULLIF(current_setting(''app.write_goal'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[]))', fk_column, target_schema, target_table, fk_child);
            ELSIF parent_has_t THEN
                write_expr := format('EXISTS (SELECT 1 FROM %I.%I AS parent JOIN proxiima_core.memory AS owner_memory ON owner_memory.t = parent.t WHERE parent.%I = %I.%I.%I AND ((owner_memory.kind = ''fact'' AND owner_memory.owner_id = ANY((SELECT COALESCE(NULLIF(current_setting(''app.write_owner'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[])) OR (owner_memory.kind = ''abstraction'' AND owner_memory.owner_id = ANY((SELECT COALESCE(NULLIF(current_setting(''app.write_abstraction'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[])) OR (owner_memory.kind = ''perspective'' AND owner_memory.owner_id = ANY((SELECT COALESCE(NULLIF(current_setting(''app.write_perspective'', true), '''')::uuid[], ''{}''::uuid[]))::uuid[]))))', fk_schema, fk_table, fk_column, target_schema, target_table, fk_child);
            ELSE
                RAISE EXCEPTION 'owner RLS writable classification missing for %.%', target_schema, target_table
                    USING ERRCODE = 'PX002';
            END IF;
    END CASE;

    select_expr := format('(%s) OR (%s)', read_expr, write_expr);
    RETURN NEXT;
END
$function$;
REVOKE ALL ON FUNCTION proxiima_core.owner_rls_class_predicates(caller text, class_label text, target_schema text, target_table text, class proxiima_core.owner_rls_class, owner_column text) FROM PUBLIC;

-- agent_derivation_v1
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'agent_derivation_v1', 'fk_parent', NULL);

-- schema_evolution is deployment metadata: only the validated platform lane
-- writes it, while serving startup reads the completed-source catalog.
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'schema_evolution', 'ownerless', NULL);

SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'schema_evolution_authority', 'ownerless', NULL);
-- Captured-publication erase links belong only to the migration/erase lane.
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'schema_evolution_publication', 'ownerless', NULL);

-- agent_note_v1
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'agent_note_v1', 'fk_parent', NULL);

-- blob
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'blob', 'owner_id', 'owner_id');

-- A publication grants only the blob explicitly cited by its exact Fact.
-- The immutable blob identity is distinct from mutable upload routing.
ALTER POLICY proxiima_owner_read ON proxiima_core.blob USING (
    owner_id=ANY(COALESCE(NULLIF(current_setting('app.owner',true),'')::uuid[],'{}'))
    OR EXISTS (SELECT 1 FROM proxiima_core.memory cited
               WHERE cited.blob_id=blob.blob_id AND cited.owner_id=blob.owner_id
                 AND cited.kind='fact' AND cited.forgotten_at IS NULL
                 AND proxiima_core.goal_memory_read_admitted(cited.t))
);

-- blob_uploads
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'blob_uploads', 'owner_id', 'owner_id');
ALTER POLICY proxiima_owner_read ON proxiima_core.blob_uploads USING (
    owner_id=ANY(COALESCE(NULLIF(current_setting('app.owner',true),'')::uuid[],'{}'))
    OR (status='completed' AND EXISTS (
        SELECT 1 FROM proxiima_core.goal_memory_publication publication
        JOIN proxiima_core.memory cited ON cited.t=publication.memory_t
        JOIN proxiima_core.goal_memory_read_routes() route
          ON route.memory_t=cited.t AND route.goal_handle=publication.goal_handle
        WHERE publication.cited_upload_id=blob_uploads.upload_id
          AND cited.blob_id=blob_uploads.blob_id AND cited.owner_id=blob_uploads.owner_id
          AND cited.kind='fact' AND cited.forgotten_at IS NULL
    ))
);


-- closed_handle
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'closed_handle', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.closed_handle USING ((EXISTS ( SELECT 1
   FROM proxiima_core.memory_head parent
  WHERE ((parent.handle = closed_handle.handle) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.read_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END))))));

ALTER POLICY proxiima_owner_write ON proxiima_core.closed_handle USING ((EXISTS ( SELECT 1
   FROM proxiima_core.memory_head parent
  WHERE ((parent.handle = closed_handle.handle) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END)))))) WITH CHECK ((EXISTS ( SELECT 1
   FROM proxiima_core.memory_head parent
  WHERE ((parent.handle = closed_handle.handle) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END))))));

-- cold_purge_pending
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'cold_purge_pending', 'owner_id', 'owner_id');

-- content
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'content', 'owner_id', 'owner_id');

ALTER POLICY proxiima_owner_read ON proxiima_core.content USING (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.read_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) OR ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND ((EXISTS ( SELECT 1
   FROM proxiima_core.memory parent
  WHERE ((parent.content_id = content.content_id) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.read_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END)))))))));

ALTER POLICY proxiima_owner_write ON proxiima_core.content USING (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) OR ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND ((EXISTS ( SELECT 1
   FROM proxiima_core.memory parent
  WHERE ((parent.content_id = content.content_id) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END))))))))) WITH CHECK ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])));

-- delegated_authority_grants
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'delegated_authority_grants', 'owner_id', 'owner_id');

-- embedding_heads
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'embedding_heads', 'owner_id', 'owner_id');

-- embedding_jobs
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'embedding_jobs', 'owner_id', 'owner_id');

-- embeddings
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'embeddings', 'owner_id', 'owner_id');

-- erased_pin_target
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'erased_pin_target', 'ownerless', NULL);

-- An edge already shows that its target existed, so a reader who can read
-- a memory pinning an erased id may learn that it was erased (#671). No
-- other owner-scope read reaches the witness.
ALTER POLICY proxiima_owner_read ON proxiima_core.erased_pin_target USING (
    current_setting('app.proxiima_scope', true) = 'owner'
    AND EXISTS (SELECT 1 FROM proxiima_core.memory pinning
                WHERE pinning.origins @> ARRAY[erased_pin_target.t]
                   OR pinning.refs @> ARRAY[erased_pin_target.t]
                   OR pinning.reads @> ARRAY[erased_pin_target.t]
                   OR pinning.forked_from = erased_pin_target.t)
);

-- spiice_surface
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'spiice_surface', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.spiice_surface USING (((current_setting('app.proxiima_scope'::text, true) = 'owner'::text) AND (cardinality(COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[])) > 0)));

-- goal
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'goal', 'owner_id', 'owner_id');

ALTER POLICY proxiima_owner_read ON proxiima_core.goal USING (
    owner_id = ANY ((SELECT COALESCE((NULLIF(current_setting('app.read_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])
);

ALTER POLICY proxiima_owner_write ON proxiima_core.goal USING ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) WITH CHECK ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])));

-- goal_motivation_index is a derived projection. Goal owners may read rows
-- through their Goal scope; only the trigger/platform path may write them.
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'goal_motivation_index', 'ownerless', NULL);
ALTER POLICY proxiima_owner_read ON proxiima_core.goal_motivation_index USING (
    owner_id = ANY (COALESCE(NULLIF(current_setting('app.read_goal', true), '')::uuid[], '{}'::uuid[])
));

-- goal_change_v1
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'goal_change_v1', 'fk_parent', NULL);

-- goal_head
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'goal_head', 'owner_id', 'owner_id');

ALTER POLICY proxiima_owner_read ON proxiima_core.goal_head USING ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.read_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])));

ALTER POLICY proxiima_owner_write ON proxiima_core.goal_head USING ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) WITH CHECK ((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])));

-- Goal collaboration routes. These are additive to owner RLS and never add a
-- subject to app.owner/app.read_* arrays. The trusted host binds app.goal_subject
-- from the sealed authentication scope. The database role is a trusted host
-- connection; neither network inputs nor spiices may supply scope SQL.
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'goal_access_grant', 'ownerless', NULL);
ALTER POLICY proxiima_owner_read ON proxiima_core.goal_access_grant USING (
    proxiima_core.goal_access_grant.subject_id = NULLIF(current_setting('app.goal_subject', true), '')::uuid
    OR EXISTS (SELECT 1 FROM proxiima_core.goal_head h
               WHERE h.handle = proxiima_core.goal_access_grant.goal_handle
                 AND h.owner_id = ANY (COALESCE(NULLIF(current_setting('app.read_goal', true), '')::uuid[], '{}'::uuid[])))
);
ALTER POLICY proxiima_owner_write ON proxiima_core.goal_access_grant USING (false) WITH CHECK (false);

SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'goal_memory_publication', 'ownerless', NULL);
ALTER POLICY proxiima_owner_read ON proxiima_core.goal_memory_publication USING (
    EXISTS (SELECT 1 FROM proxiima_core.goal_memory_read_routes() route
             WHERE route.goal_handle=goal_memory_publication.goal_handle
               AND route.memory_t=goal_memory_publication.memory_t)
    OR EXISTS (SELECT 1 FROM proxiima_core.memory source
                WHERE source.t=goal_memory_publication.memory_t
                  AND source.owner_id=ANY(CASE source.kind
                      WHEN 'fact' THEN COALESCE(NULLIF(current_setting('app.owner',true),'')::uuid[],'{}')
                      WHEN 'abstraction' THEN COALESCE(NULLIF(current_setting('app.read_abstraction',true),'')::uuid[],'{}')
                      WHEN 'perspective' THEN COALESCE(NULLIF(current_setting('app.read_perspective',true),'')::uuid[],'{}') END))
);
ALTER POLICY proxiima_owner_write ON proxiima_core.goal_memory_publication USING (false) WITH CHECK (false);

SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'goal_work_binding', 'ownerless', NULL);
ALTER POLICY proxiima_owner_read ON proxiima_core.goal_work_binding USING (
    EXISTS (SELECT 1 FROM proxiima_core.goal_memory_read_routes() route
             WHERE route.goal_handle=goal_work_binding.goal_handle
               AND route.memory_handle=goal_work_binding.memory_handle)
    OR EXISTS (SELECT 1 FROM proxiima_core.memory_head h WHERE h.handle=goal_work_binding.memory_handle)
);
ALTER POLICY proxiima_owner_write ON proxiima_core.goal_work_binding USING (false) WITH CHECK (false);

-- goal_replay_declaration
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'goal_replay_declaration', 'fk_parent', NULL);

-- group_edges
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'group_edges', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.group_edges USING (((child_group_id = ANY (COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))) OR (parent_group_id = ANY (COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[])))));

ALTER POLICY proxiima_owner_write ON proxiima_core.group_edges USING (((parent_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))) OR (child_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))))) WITH CHECK (((parent_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))) AND (child_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[])))));

-- group_lock_request
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'group_lock_request', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.group_lock_request USING (((left_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))) OR (right_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[])))));

ALTER POLICY proxiima_owner_write ON proxiima_core.group_lock_request USING (((left_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))) OR (right_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))))) WITH CHECK (((left_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))) OR (right_group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[])))));

-- group_memberships
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'group_memberships', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.group_memberships USING ((group_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])));

ALTER POLICY proxiima_owner_write ON proxiima_core.group_memberships USING ((group_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) WITH CHECK ((group_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])));

-- group_role_projection
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'group_role_projection', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.group_role_projection USING (
    group_id = ANY (COALESCE(NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[]))
    OR (member_user_id = NULLIF(current_setting('app.authority_subject', true), '')::uuid
        AND member_user_id = NULLIF(current_setting('app.subject', true), '')::uuid)
);

ALTER POLICY proxiima_owner_write ON proxiima_core.group_role_projection USING ((group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[])))) WITH CHECK ((group_id = ANY (COALESCE((NULLIF(current_setting('app.manage_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]))));

-- ingest_keys
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'ingest_keys', 'owner_id', 'owner_id');

-- installation
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'installation', 'ownerless', NULL);

-- interpretation_v1
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'interpretation_v1', 'fk_parent', NULL);

-- lexical_default
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'lexical_default', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.lexical_default USING (((current_setting('app.proxiima_scope'::text, true) = 'owner'::text) AND (cardinality(COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[])) > 0)));

-- lexical_languages
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'lexical_languages', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.lexical_languages USING (((current_setting('app.proxiima_scope'::text, true) = 'owner'::text) AND (cardinality(COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[])) > 0)));

ALTER POLICY proxiima_owner_write ON proxiima_core.lexical_languages USING (false) WITH CHECK (((current_setting('app.proxiima_scope'::text, true) = 'owner'::text) AND (cardinality(COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[])) > 0)));

-- memory
-- A forgotten Memory (forgotten_at set) is hidden from owner reads and
-- writes, and from every sidecar policy that reaches it through memory, unless
-- the transaction runs in the lifecycle scope (PgLifecycleScope, #488).
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'memory', 'owner_id', 'owner_id');

ALTER POLICY proxiima_owner_read ON proxiima_core.memory USING (
    (((owner_id = ANY ((SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (((kind = 'fact'::proxiima_core.memory_kind) AND (owner_id = ANY ((SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) OR ((kind = 'abstraction'::proxiima_core.memory_kind) AND (owner_id = ANY ((SELECT COALESCE((NULLIF(current_setting('app.read_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) OR ((kind = 'perspective'::proxiima_core.memory_kind) AND (owner_id = ANY ((SELECT COALESCE((NULLIF(current_setting('app.read_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))))) AND ((forgotten_at IS NULL) OR (current_setting('app.proxiima_lifecycle'::text, true) = 'on'::text)))
);

ALTER POLICY proxiima_owner_write ON proxiima_core.memory USING ((((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (((kind = 'fact'::proxiima_core.memory_kind) AND (owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) OR ((kind = 'abstraction'::proxiima_core.memory_kind) AND (owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) OR ((kind = 'perspective'::proxiima_core.memory_kind) AND (owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))))) AND ((forgotten_at IS NULL) OR (current_setting('app.proxiima_lifecycle'::text, true) = 'on'::text)))) WITH CHECK ((((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (((kind = 'fact'::proxiima_core.memory_kind) AND (owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) OR ((kind = 'abstraction'::proxiima_core.memory_kind) AND (owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))) OR ((kind = 'perspective'::proxiima_core.memory_kind) AND (owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[]))))) AND ((forgotten_at IS NULL) OR (current_setting('app.proxiima_lifecycle'::text, true) = 'on'::text))));

-- memory_external_origin
-- Reached through the memory it describes (#478). Forget keeps the memory
-- row (#488), so a forgotten parent still has its owner; owner reads hide it
-- with the parent, lifecycle reads (export, erase, transfer) see it.
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class memory_owner', 'proxiima_core', 'memory_external_origin', 'memory_owner', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.memory_external_origin USING (((EXISTS ( SELECT 1
   FROM proxiima_core.memory parent
  WHERE ((parent.t = memory_external_origin.t) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.read_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END)))))));

ALTER POLICY proxiima_owner_write ON proxiima_core.memory_external_origin USING (((EXISTS ( SELECT 1
   FROM proxiima_core.memory parent
  WHERE ((parent.t = memory_external_origin.t) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END))))))) WITH CHECK ((EXISTS ( SELECT 1
   FROM proxiima_core.memory parent
  WHERE ((parent.t = memory_external_origin.t) AND (parent.owner_id = ANY (
        CASE (parent.kind)::text
            WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
            ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
        END))))));

-- memory_head
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'memory_head', 'owner_id', 'owner_id');

ALTER POLICY proxiima_owner_read ON proxiima_core.memory_head USING (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (owner_id = ANY (
CASE (kind)::text
    WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    ELSE ( SELECT COALESCE((NULLIF(current_setting('app.read_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
END))));

ALTER POLICY proxiima_owner_write ON proxiima_core.memory_head USING (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (owner_id = ANY (
CASE (kind)::text
    WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
END)))) WITH CHECK (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (owner_id = ANY (
CASE (kind)::text
    WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
END))));

-- owner_rls_epoch
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'owner_rls_epoch', 'ownerless', NULL);

-- owners
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'owners', 'owner_id', 'owner_id');

ALTER POLICY proxiima_owner_read ON proxiima_core.owners USING (
    owner_id = ANY (COALESCE(NULLIF(current_setting('app.owner', true), '')::uuid[], '{}'::uuid[]))
    OR (owner_id = NULLIF(current_setting('app.authority_subject', true), '')::uuid
        AND owner_id = NULLIF(current_setting('app.subject', true), '')::uuid)
);

-- projection
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'projection', 'owner_id', 'owner_id');

-- publication_consumer
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'publication_consumer', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.publication_consumer USING (((current_setting('app.proxiima_scope'::text, true) = 'owner'::text) AND (cardinality(COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[])) > 0)));

-- publication_backlog: the publisher's estimate, read by every listenable
-- write whatever its owner, like publication_consumer.
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class ownerless', 'proxiima_core', 'publication_backlog', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.publication_backlog USING (((current_setting('app.proxiima_scope'::text, true) = 'owner'::text) AND (cardinality(COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[])) > 0)));

-- publication_delivery
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'publication_delivery', 'fk_parent', NULL);

-- publication_origin
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'publication_origin', 'owner_id', 'original_owner_id');

-- publication_outbox
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'publication_outbox', 'owner_id', 'owner_id');

-- sketch
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'sketch', 'owner_id', 'owner_id');

ALTER POLICY proxiima_owner_read ON proxiima_core.sketch USING (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (owner_id = ANY (
CASE (kind)::text
    WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.read_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    ELSE ( SELECT COALESCE((NULLIF(current_setting('app.read_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
END))));

ALTER POLICY proxiima_owner_write ON proxiima_core.sketch USING (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (owner_id = ANY (
CASE (kind)::text
    WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
END)))) WITH CHECK (((owner_id = ANY (( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")::uuid[])) AND (owner_id = ANY (
CASE (kind)::text
    WHEN 'fact'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_owner'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'abstraction'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_abstraction'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    WHEN 'perspective'::text THEN ( SELECT COALESCE((NULLIF(current_setting('app.write_perspective'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
    ELSE ( SELECT COALESCE((NULLIF(current_setting('app.write_goal'::text, true), ''::text))::uuid[], '{}'::uuid[]) AS "coalesce")
END))));

-- retention
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'retention', 'owner_id', 'owner_id');

-- source_cursors
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'source_cursors', 'owner_id', 'owner_id');

-- task_goal_v1
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'task_goal_v1', 'fk_parent', NULL);

-- utterance_v1
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'utterance_v1', 'fk_parent', NULL);

-- write_act_v1
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class fk_parent', 'proxiima_core', 'write_act_v1', 'fk_parent', NULL);

-- store_identity
SELECT proxiima_core.owner_rls_apply_class(
    'policies.sql', 'class ownerless', 'proxiima_core', 'store_identity', 'ownerless', NULL);

-- publication_counter
SELECT proxiima_core.owner_rls_apply_class(
    'policies.sql', 'class owner_id', 'proxiima_core', 'publication_counter', 'owner_id', 'owner_id');

-- owner_follows: the host writes it (platform); owner scopes may read it,
-- like spiice_surface.
SELECT proxiima_core.owner_rls_apply_class(
    'policies.sql', 'class ownerless', 'proxiima_core', 'owner_follows', 'ownerless', NULL);

ALTER POLICY proxiima_owner_read ON proxiima_core.owner_follows USING (((current_setting('app.proxiima_scope'::text, true) = 'owner'::text) AND (cardinality(COALESCE((NULLIF(current_setting('app.owner'::text, true), ''::text))::uuid[], '{}'::uuid[])) > 0)));

-- export_erase_fence and maintenance_lease: platform coordination state about
-- no owner. Only the platform scope reaches them.
SELECT proxiima_core.owner_rls_apply_class(
    'policies.sql', 'class ownerless', 'proxiima_core', 'export_erase_fence', 'ownerless', NULL);

SELECT proxiima_core.owner_rls_apply_class(
    'policies.sql', 'class ownerless', 'proxiima_core', 'maintenance_lease', 'ownerless', NULL);

-- vector_index_debt: read and written at its entity's kind, as its head is;
-- the platform scope drains it.
SELECT proxiima_core.owner_rls_apply_class('policies.sql', 'class owner_id', 'proxiima_core', 'vector_index_debt', 'owner_id', 'owner_id');

-- Add exact Goal routes after the owner predicates. A head remains owner-only:
-- publishing an old version never reveals its private global successor.
DO $goal_routes$
DECLARE relation_name text; predicate text; extra text;
BEGIN
    FOREACH relation_name IN ARRAY ARRAY['memory', 'goal', 'goal_head', 'content', 'sketch'] LOOP
        SELECT pg_get_expr(p.polqual, p.polrelid) INTO predicate
          FROM pg_policy p
         WHERE p.polrelid = format('proxiima_core.%I', relation_name)::regclass
           AND p.polname = 'proxiima_owner_read';
        extra := CASE relation_name
          WHEN 'memory' THEN 'proxiima_core.goal_memory_read_admitted(t)'
          WHEN 'goal' THEN 'proxiima_core.goal_read_admitted(t)'
          WHEN 'goal_head' THEN 'proxiima_core.goal_read_admitted(t)'
          WHEN 'content' THEN 'EXISTS (SELECT 1 FROM proxiima_core.memory m WHERE m.content_id = content.content_id AND m.owner_id = content.owner_id AND proxiima_core.goal_memory_read_admitted(m.t))'
          WHEN 'sketch' THEN 'EXISTS (SELECT 1 FROM proxiima_core.memory m WHERE m.t=sketch.t AND m.owner_id=sketch.owner_id AND proxiima_core.goal_memory_read_admitted(m.t)) OR EXISTS (SELECT 1 FROM proxiima_core.goal g WHERE g.t=sketch.t AND g.owner_id=sketch.owner_id AND proxiima_core.goal_read_admitted(g.t))'
        END;
        EXECUTE format('ALTER POLICY proxiima_owner_read ON proxiima_core.%I USING ((%s) OR (%s))', relation_name, predicate, extra);
    END LOOP;
END
$goal_routes$;
