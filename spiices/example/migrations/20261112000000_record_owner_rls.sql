-- Install the owner policies of every table through the current installer,
-- which records each table's class for boot.
SELECT proxiima_core.install_owner_rls(
    'proxiima_example',
    ARRAY['notebooks'],
    ARRAY['note_v1', 'digest_v1'],
    ARRAY['settings'],
    ARRAY['stamp_v1']
);
