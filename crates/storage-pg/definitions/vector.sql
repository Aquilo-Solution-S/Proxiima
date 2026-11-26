-- Vector candidate functions of a store that keeps its vectors in pgvector.
-- Applied by the definitions pass only for that store kind, after functions.sql
-- (generated: vector_index.rs). A store with a host vector index never applies
-- this file and needs no `vector` extension.

DO $install_candidate$
DECLARE original_role name := current_user;
BEGIN
GRANT USAGE, CREATE ON SCHEMA proxiima_core TO proxiima_search;
GRANT SELECT ON proxiima_core.embeddings, proxiima_core.embedding_heads, proxiima_core.memory TO proxiima_search;
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embeddings;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embeddings FOR SELECT TO proxiima_search USING (true);
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embedding_heads;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embedding_heads FOR SELECT TO proxiima_search USING (true);
SET LOCAL ROLE proxiima_search;
EXECUTE $generated_function$DROP FUNCTION IF EXISTS proxiima_core.vector_candidates_d384(uuid[], text, public.vector, bigint);
                 CREATE OR REPLACE FUNCTION proxiima_core.vector_candidates_d384(uuid[], text, public.vector, bigint, boolean)
                 RETURNS TABLE(memory_id uuid, similarity_score real)
                 LANGUAGE plpgsql STABLE SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 SET plan_cache_mode = force_custom_plan
                 AS $body$
DECLARE
    target bigint := LEAST(GREATEST($4, 0), 1000);
    chunk_window bigint := GREATEST(target, 1);
    candidate_ids uuid[] := '{}'::uuid[];
    scanned_ids uuid[];
    fetched bigint;
    exact_budget bigint;
    owned bigint;
BEGIN
    PERFORM proxiima_core.assert_search_scope($1);
    IF public.vector_dims($3) IS DISTINCT FROM 384 THEN
        RAISE EXCEPTION 'query vector does not match index width'
            USING ERRCODE = 'PX002';
    END IF;
    IF target = 0 THEN
        RETURN;
    END IF;
    SELECT LEAST(floor(sqrt(108 * target * GREATEST(c.reltuples, target))), 32000)::bigint
      INTO exact_budget
      FROM pg_catalog.pg_class c
     WHERE c.oid = 'proxiima_core.embeddings_hnsw_d384'::regclass;
    SELECT count(*) INTO owned
      FROM (SELECT 1 FROM proxiima_core.embeddings emb
             WHERE emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND emb.dim = 384
             LIMIT exact_budget + 1) owned_chunks;
    IF owned > exact_budget THEN
        LOOP
            WITH scanned AS MATERIALIZED (
            SELECT emb.entity_id
              FROM proxiima_core.embeddings emb
             WHERE emb.dim = 384
               AND emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.embedding_heads head
                    WHERE head.entity_id = emb.entity_id
                      AND head.model_id = emb.model_id
                      AND head.dim = emb.dim
                      AND head.embedding_version = emb.embedding_version
               )
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.live_memory m
                    WHERE m.t = emb.entity_id
                      AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
               )
             ORDER BY emb.vec::public.vector(384) OPERATOR(public.<=>) $3::public.vector(384)
             LIMIT chunk_window
            )
            SELECT COALESCE(array_agg(DISTINCT s.entity_id), '{}'::uuid[]), count(*)
              INTO scanned_ids, fetched FROM scanned s;
            SELECT ARRAY(SELECT DISTINCT id FROM unnest(candidate_ids || scanned_ids) id)
              INTO candidate_ids;
            EXIT WHEN fetched < chunk_window
                   OR cardinality(candidate_ids) >= target
                   OR chunk_window >= 1000;
            chunk_window := LEAST(chunk_window * 2, 1000);
        END LOOP;
        IF cardinality(candidate_ids) >= target THEN
            RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 384
                   AND emb.entity_id = ANY(candidate_ids)
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
            RETURN;
        END IF;
    END IF;
    RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 384
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
END;
$body$;$generated_function$;
REVOKE ALL ON FUNCTION proxiima_core.vector_candidates_d384(uuid[], text, public.vector, bigint, boolean) FROM PUBLIC;
EXECUTE format('SET LOCAL ROLE %I', original_role);
REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search;
END
$install_candidate$;

DO $install_candidate$
DECLARE original_role name := current_user;
BEGIN
GRANT USAGE, CREATE ON SCHEMA proxiima_core TO proxiima_search;
GRANT SELECT ON proxiima_core.embeddings, proxiima_core.embedding_heads, proxiima_core.memory TO proxiima_search;
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embeddings;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embeddings FOR SELECT TO proxiima_search USING (true);
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embedding_heads;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embedding_heads FOR SELECT TO proxiima_search USING (true);
SET LOCAL ROLE proxiima_search;
EXECUTE $generated_function$DROP FUNCTION IF EXISTS proxiima_core.vector_candidates_d768(uuid[], text, public.vector, bigint);
                 CREATE OR REPLACE FUNCTION proxiima_core.vector_candidates_d768(uuid[], text, public.vector, bigint, boolean)
                 RETURNS TABLE(memory_id uuid, similarity_score real)
                 LANGUAGE plpgsql STABLE SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 SET plan_cache_mode = force_custom_plan
                 AS $body$
DECLARE
    target bigint := LEAST(GREATEST($4, 0), 1000);
    chunk_window bigint := GREATEST(target, 1);
    candidate_ids uuid[] := '{}'::uuid[];
    scanned_ids uuid[];
    fetched bigint;
    exact_budget bigint;
    owned bigint;
BEGIN
    PERFORM proxiima_core.assert_search_scope($1);
    IF public.vector_dims($3) IS DISTINCT FROM 768 THEN
        RAISE EXCEPTION 'query vector does not match index width'
            USING ERRCODE = 'PX002';
    END IF;
    IF target = 0 THEN
        RETURN;
    END IF;
    SELECT LEAST(floor(sqrt(108 * target * GREATEST(c.reltuples, target))), 32000)::bigint
      INTO exact_budget
      FROM pg_catalog.pg_class c
     WHERE c.oid = 'proxiima_core.embeddings_hnsw_d768'::regclass;
    SELECT count(*) INTO owned
      FROM (SELECT 1 FROM proxiima_core.embeddings emb
             WHERE emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND emb.dim = 768
             LIMIT exact_budget + 1) owned_chunks;
    IF owned > exact_budget THEN
        LOOP
            WITH scanned AS MATERIALIZED (
            SELECT emb.entity_id
              FROM proxiima_core.embeddings emb
             WHERE emb.dim = 768
               AND emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.embedding_heads head
                    WHERE head.entity_id = emb.entity_id
                      AND head.model_id = emb.model_id
                      AND head.dim = emb.dim
                      AND head.embedding_version = emb.embedding_version
               )
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.live_memory m
                    WHERE m.t = emb.entity_id
                      AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
               )
             ORDER BY emb.vec::public.vector(768) OPERATOR(public.<=>) $3::public.vector(768)
             LIMIT chunk_window
            )
            SELECT COALESCE(array_agg(DISTINCT s.entity_id), '{}'::uuid[]), count(*)
              INTO scanned_ids, fetched FROM scanned s;
            SELECT ARRAY(SELECT DISTINCT id FROM unnest(candidate_ids || scanned_ids) id)
              INTO candidate_ids;
            EXIT WHEN fetched < chunk_window
                   OR cardinality(candidate_ids) >= target
                   OR chunk_window >= 1000;
            chunk_window := LEAST(chunk_window * 2, 1000);
        END LOOP;
        IF cardinality(candidate_ids) >= target THEN
            RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 768
                   AND emb.entity_id = ANY(candidate_ids)
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
            RETURN;
        END IF;
    END IF;
    RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 768
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
END;
$body$;$generated_function$;
REVOKE ALL ON FUNCTION proxiima_core.vector_candidates_d768(uuid[], text, public.vector, bigint, boolean) FROM PUBLIC;
EXECUTE format('SET LOCAL ROLE %I', original_role);
REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search;
END
$install_candidate$;

DO $install_candidate$
DECLARE original_role name := current_user;
BEGIN
GRANT USAGE, CREATE ON SCHEMA proxiima_core TO proxiima_search;
GRANT SELECT ON proxiima_core.embeddings, proxiima_core.embedding_heads, proxiima_core.memory TO proxiima_search;
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embeddings;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embeddings FOR SELECT TO proxiima_search USING (true);
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embedding_heads;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embedding_heads FOR SELECT TO proxiima_search USING (true);
SET LOCAL ROLE proxiima_search;
EXECUTE $generated_function$DROP FUNCTION IF EXISTS proxiima_core.vector_candidates_d1024(uuid[], text, public.vector, bigint);
                 CREATE OR REPLACE FUNCTION proxiima_core.vector_candidates_d1024(uuid[], text, public.vector, bigint, boolean)
                 RETURNS TABLE(memory_id uuid, similarity_score real)
                 LANGUAGE plpgsql STABLE SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 SET plan_cache_mode = force_custom_plan
                 AS $body$
DECLARE
    target bigint := LEAST(GREATEST($4, 0), 1000);
    chunk_window bigint := GREATEST(target, 1);
    candidate_ids uuid[] := '{}'::uuid[];
    scanned_ids uuid[];
    fetched bigint;
    exact_budget bigint;
    owned bigint;
BEGIN
    PERFORM proxiima_core.assert_search_scope($1);
    IF public.vector_dims($3) IS DISTINCT FROM 1024 THEN
        RAISE EXCEPTION 'query vector does not match index width'
            USING ERRCODE = 'PX002';
    END IF;
    IF target = 0 THEN
        RETURN;
    END IF;
    SELECT LEAST(floor(sqrt(108 * target * GREATEST(c.reltuples, target))), 32000)::bigint
      INTO exact_budget
      FROM pg_catalog.pg_class c
     WHERE c.oid = 'proxiima_core.embeddings_hnsw_d1024'::regclass;
    SELECT count(*) INTO owned
      FROM (SELECT 1 FROM proxiima_core.embeddings emb
             WHERE emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND emb.dim = 1024
             LIMIT exact_budget + 1) owned_chunks;
    IF owned > exact_budget THEN
        LOOP
            WITH scanned AS MATERIALIZED (
            SELECT emb.entity_id
              FROM proxiima_core.embeddings emb
             WHERE emb.dim = 1024
               AND emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.embedding_heads head
                    WHERE head.entity_id = emb.entity_id
                      AND head.model_id = emb.model_id
                      AND head.dim = emb.dim
                      AND head.embedding_version = emb.embedding_version
               )
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.live_memory m
                    WHERE m.t = emb.entity_id
                      AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
               )
             ORDER BY emb.vec::public.vector(1024) OPERATOR(public.<=>) $3::public.vector(1024)
             LIMIT chunk_window
            )
            SELECT COALESCE(array_agg(DISTINCT s.entity_id), '{}'::uuid[]), count(*)
              INTO scanned_ids, fetched FROM scanned s;
            SELECT ARRAY(SELECT DISTINCT id FROM unnest(candidate_ids || scanned_ids) id)
              INTO candidate_ids;
            EXIT WHEN fetched < chunk_window
                   OR cardinality(candidate_ids) >= target
                   OR chunk_window >= 1000;
            chunk_window := LEAST(chunk_window * 2, 1000);
        END LOOP;
        IF cardinality(candidate_ids) >= target THEN
            RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 1024
                   AND emb.entity_id = ANY(candidate_ids)
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
            RETURN;
        END IF;
    END IF;
    RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 1024
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
END;
$body$;$generated_function$;
REVOKE ALL ON FUNCTION proxiima_core.vector_candidates_d1024(uuid[], text, public.vector, bigint, boolean) FROM PUBLIC;
EXECUTE format('SET LOCAL ROLE %I', original_role);
REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search;
END
$install_candidate$;

DO $install_candidate$
DECLARE original_role name := current_user;
BEGIN
GRANT USAGE, CREATE ON SCHEMA proxiima_core TO proxiima_search;
GRANT SELECT ON proxiima_core.embeddings, proxiima_core.embedding_heads, proxiima_core.memory TO proxiima_search;
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embeddings;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embeddings FOR SELECT TO proxiima_search USING (true);
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embedding_heads;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embedding_heads FOR SELECT TO proxiima_search USING (true);
SET LOCAL ROLE proxiima_search;
EXECUTE $generated_function$DROP FUNCTION IF EXISTS proxiima_core.vector_candidates_d1536(uuid[], text, public.vector, bigint);
                 CREATE OR REPLACE FUNCTION proxiima_core.vector_candidates_d1536(uuid[], text, public.vector, bigint, boolean)
                 RETURNS TABLE(memory_id uuid, similarity_score real)
                 LANGUAGE plpgsql STABLE SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 SET plan_cache_mode = force_custom_plan
                 AS $body$
DECLARE
    target bigint := LEAST(GREATEST($4, 0), 1000);
    chunk_window bigint := GREATEST(target, 1);
    candidate_ids uuid[] := '{}'::uuid[];
    scanned_ids uuid[];
    fetched bigint;
    exact_budget bigint;
    owned bigint;
BEGIN
    PERFORM proxiima_core.assert_search_scope($1);
    IF public.vector_dims($3) IS DISTINCT FROM 1536 THEN
        RAISE EXCEPTION 'query vector does not match index width'
            USING ERRCODE = 'PX002';
    END IF;
    IF target = 0 THEN
        RETURN;
    END IF;
    SELECT LEAST(floor(sqrt(108 * target * GREATEST(c.reltuples, target))), 32000)::bigint
      INTO exact_budget
      FROM pg_catalog.pg_class c
     WHERE c.oid = 'proxiima_core.embeddings_hnsw_d1536'::regclass;
    SELECT count(*) INTO owned
      FROM (SELECT 1 FROM proxiima_core.embeddings emb
             WHERE emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND emb.dim = 1536
             LIMIT exact_budget + 1) owned_chunks;
    IF owned > exact_budget THEN
        LOOP
            WITH scanned AS MATERIALIZED (
            SELECT emb.entity_id
              FROM proxiima_core.embeddings emb
             WHERE emb.dim = 1536
               AND emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.embedding_heads head
                    WHERE head.entity_id = emb.entity_id
                      AND head.model_id = emb.model_id
                      AND head.dim = emb.dim
                      AND head.embedding_version = emb.embedding_version
               )
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.live_memory m
                    WHERE m.t = emb.entity_id
                      AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
               )
             ORDER BY emb.vec::public.vector(1536) OPERATOR(public.<=>) $3::public.vector(1536)
             LIMIT chunk_window
            )
            SELECT COALESCE(array_agg(DISTINCT s.entity_id), '{}'::uuid[]), count(*)
              INTO scanned_ids, fetched FROM scanned s;
            SELECT ARRAY(SELECT DISTINCT id FROM unnest(candidate_ids || scanned_ids) id)
              INTO candidate_ids;
            EXIT WHEN fetched < chunk_window
                   OR cardinality(candidate_ids) >= target
                   OR chunk_window >= 1000;
            chunk_window := LEAST(chunk_window * 2, 1000);
        END LOOP;
        IF cardinality(candidate_ids) >= target THEN
            RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 1536
                   AND emb.entity_id = ANY(candidate_ids)
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
            RETURN;
        END IF;
    END IF;
    RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 1536
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
END;
$body$;$generated_function$;
REVOKE ALL ON FUNCTION proxiima_core.vector_candidates_d1536(uuid[], text, public.vector, bigint, boolean) FROM PUBLIC;
EXECUTE format('SET LOCAL ROLE %I', original_role);
REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search;
END
$install_candidate$;

DO $install_candidate$
DECLARE original_role name := current_user;
BEGIN
GRANT USAGE, CREATE ON SCHEMA proxiima_core TO proxiima_search;
GRANT SELECT ON proxiima_core.embeddings, proxiima_core.embedding_heads, proxiima_core.memory TO proxiima_search;
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embeddings;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embeddings FOR SELECT TO proxiima_search USING (true);
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embedding_heads;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embedding_heads FOR SELECT TO proxiima_search USING (true);
SET LOCAL ROLE proxiima_search;
EXECUTE $generated_function$DROP FUNCTION IF EXISTS proxiima_core.vector_candidates_d2048(uuid[], text, public.vector, bigint);
                 CREATE OR REPLACE FUNCTION proxiima_core.vector_candidates_d2048(uuid[], text, public.vector, bigint, boolean)
                 RETURNS TABLE(memory_id uuid, similarity_score real)
                 LANGUAGE plpgsql STABLE SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 SET plan_cache_mode = force_custom_plan
                 AS $body$
DECLARE
    target bigint := LEAST(GREATEST($4, 0), 1000);
    chunk_window bigint := GREATEST(target, 1);
    candidate_ids uuid[] := '{}'::uuid[];
    scanned_ids uuid[];
    fetched bigint;
    exact_budget bigint;
    owned bigint;
BEGIN
    PERFORM proxiima_core.assert_search_scope($1);
    IF public.vector_dims($3) IS DISTINCT FROM 2048 THEN
        RAISE EXCEPTION 'query vector does not match index width'
            USING ERRCODE = 'PX002';
    END IF;
    IF target = 0 THEN
        RETURN;
    END IF;
    SELECT LEAST(floor(sqrt(108 * target * GREATEST(c.reltuples, target))), 32000)::bigint
      INTO exact_budget
      FROM pg_catalog.pg_class c
     WHERE c.oid = 'proxiima_core.embeddings_hnsw_d2048'::regclass;
    SELECT count(*) INTO owned
      FROM (SELECT 1 FROM proxiima_core.embeddings emb
             WHERE emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND emb.dim = 2048
             LIMIT exact_budget + 1) owned_chunks;
    IF owned > exact_budget THEN
        LOOP
            WITH scanned AS MATERIALIZED (
            SELECT emb.entity_id
              FROM proxiima_core.embeddings emb
             WHERE emb.dim = 2048
               AND emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.embedding_heads head
                    WHERE head.entity_id = emb.entity_id
                      AND head.model_id = emb.model_id
                      AND head.dim = emb.dim
                      AND head.embedding_version = emb.embedding_version
               )
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.live_memory m
                    WHERE m.t = emb.entity_id
                      AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
               )
             ORDER BY emb.vec::public.halfvec(2048) OPERATOR(public.<=>) $3::public.halfvec(2048)
             LIMIT chunk_window
            )
            SELECT COALESCE(array_agg(DISTINCT s.entity_id), '{}'::uuid[]), count(*)
              INTO scanned_ids, fetched FROM scanned s;
            SELECT ARRAY(SELECT DISTINCT id FROM unnest(candidate_ids || scanned_ids) id)
              INTO candidate_ids;
            EXIT WHEN fetched < chunk_window
                   OR cardinality(candidate_ids) >= target
                   OR chunk_window >= 1000;
            chunk_window := LEAST(chunk_window * 2, 1000);
        END LOOP;
        IF cardinality(candidate_ids) >= target THEN
            RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 2048
                   AND emb.entity_id = ANY(candidate_ids)
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
            RETURN;
        END IF;
    END IF;
    RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 2048
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
END;
$body$;$generated_function$;
REVOKE ALL ON FUNCTION proxiima_core.vector_candidates_d2048(uuid[], text, public.vector, bigint, boolean) FROM PUBLIC;
EXECUTE format('SET LOCAL ROLE %I', original_role);
REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search;
END
$install_candidate$;

DO $install_candidate$
DECLARE original_role name := current_user;
BEGIN
GRANT USAGE, CREATE ON SCHEMA proxiima_core TO proxiima_search;
GRANT SELECT ON proxiima_core.embeddings, proxiima_core.embedding_heads, proxiima_core.memory TO proxiima_search;
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embeddings;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embeddings FOR SELECT TO proxiima_search USING (true);
DROP POLICY IF EXISTS proxiima_search_candidates ON proxiima_core.embedding_heads;
CREATE POLICY proxiima_search_candidates ON proxiima_core.embedding_heads FOR SELECT TO proxiima_search USING (true);
SET LOCAL ROLE proxiima_search;
EXECUTE $generated_function$DROP FUNCTION IF EXISTS proxiima_core.vector_candidates_d3072(uuid[], text, public.vector, bigint);
                 CREATE OR REPLACE FUNCTION proxiima_core.vector_candidates_d3072(uuid[], text, public.vector, bigint, boolean)
                 RETURNS TABLE(memory_id uuid, similarity_score real)
                 LANGUAGE plpgsql STABLE SECURITY DEFINER
                 SET search_path = pg_catalog, pg_temp
                 SET plan_cache_mode = force_custom_plan
                 AS $body$
DECLARE
    target bigint := LEAST(GREATEST($4, 0), 1000);
    chunk_window bigint := GREATEST(target, 1);
    candidate_ids uuid[] := '{}'::uuid[];
    scanned_ids uuid[];
    fetched bigint;
    exact_budget bigint;
    owned bigint;
BEGIN
    PERFORM proxiima_core.assert_search_scope($1);
    IF public.vector_dims($3) IS DISTINCT FROM 3072 THEN
        RAISE EXCEPTION 'query vector does not match index width'
            USING ERRCODE = 'PX002';
    END IF;
    IF target = 0 THEN
        RETURN;
    END IF;
    SELECT LEAST(floor(sqrt(108 * target * GREATEST(c.reltuples, target))), 32000)::bigint
      INTO exact_budget
      FROM pg_catalog.pg_class c
     WHERE c.oid = 'proxiima_core.embeddings_hnsw_d3072'::regclass;
    SELECT count(*) INTO owned
      FROM (SELECT 1 FROM proxiima_core.embeddings emb
             WHERE emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND emb.dim = 3072
             LIMIT exact_budget + 1) owned_chunks;
    IF owned > exact_budget THEN
        LOOP
            WITH scanned AS MATERIALIZED (
            SELECT emb.entity_id
              FROM proxiima_core.embeddings emb
             WHERE emb.dim = 3072
               AND emb.owner_id = ANY($1)
               AND emb.model_id = $2
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.embedding_heads head
                    WHERE head.entity_id = emb.entity_id
                      AND head.model_id = emb.model_id
                      AND head.dim = emb.dim
                      AND head.embedding_version = emb.embedding_version
               )
               AND EXISTS (
                   SELECT 1 FROM proxiima_core.live_memory m
                    WHERE m.t = emb.entity_id
                      AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
               )
             ORDER BY emb.vec::public.halfvec(3072) OPERATOR(public.<=>) $3::public.halfvec(3072)
             LIMIT chunk_window
            )
            SELECT COALESCE(array_agg(DISTINCT s.entity_id), '{}'::uuid[]), count(*)
              INTO scanned_ids, fetched FROM scanned s;
            SELECT ARRAY(SELECT DISTINCT id FROM unnest(candidate_ids || scanned_ids) id)
              INTO candidate_ids;
            EXIT WHEN fetched < chunk_window
                   OR cardinality(candidate_ids) >= target
                   OR chunk_window >= 1000;
            chunk_window := LEAST(chunk_window * 2, 1000);
        END LOOP;
        IF cardinality(candidate_ids) >= target THEN
            RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 3072
                   AND emb.entity_id = ANY(candidate_ids)
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
            RETURN;
        END IF;
    END IF;
    RETURN QUERY
        SELECT s.entity_id, s.score
          FROM (SELECT emb.entity_id, emb.model_id, emb.dim, emb.embedding_version,
                       LEAST(1.0, GREATEST(0.0, MAX(
                           NULLIF(1 - (emb.vec OPERATOR(public.<=>) $3), 'NaN'::float8)
                       )))::real AS score
                  FROM proxiima_core.embeddings emb
                 WHERE emb.owner_id = ANY($1)
                   AND emb.model_id = $2
                   AND emb.dim = 3072
                 GROUP BY emb.entity_id, emb.model_id, emb.dim, emb.embedding_version
                 ORDER BY score DESC, emb.entity_id DESC) s
         CROSS JOIN LATERAL (
               SELECT 1 FROM proxiima_core.embedding_heads head
                 JOIN proxiima_core.live_memory m ON m.t = head.entity_id
                WHERE head.entity_id = s.entity_id
                  AND head.model_id = s.model_id
                  AND head.dim = s.dim
                  AND head.embedding_version = s.embedding_version
                  AND m.owner_id = ANY($1)
           AND (NOT $5::boolean
                OR (NULLIF(current_setting('app.goal_subject', true), '') IS NOT NULL
                    AND m.t IN (SELECT memory_t FROM proxiima_core.goal_visible_memory_heads()))
                OR EXISTS (SELECT 1 FROM proxiima_core.memory_head mh
                            WHERE mh.owner_id=m.owner_id AND mh.t=m.t AND mh.handle=m.handle))
                LIMIT 1) eligible
         ORDER BY s.score DESC, s.entity_id DESC
         LIMIT target;
END;
$body$;$generated_function$;
REVOKE ALL ON FUNCTION proxiima_core.vector_candidates_d3072(uuid[], text, public.vector, bigint, boolean) FROM PUBLIC;
EXECUTE format('SET LOCAL ROLE %I', original_role);
REVOKE CREATE ON SCHEMA proxiima_core FROM proxiima_search;
END
$install_candidate$;
