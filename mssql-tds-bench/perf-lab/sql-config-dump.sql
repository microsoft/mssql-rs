-- sql-config-dump.sql — Snapshot the SQL Server configuration that matters for
-- perf runs so we can validate the instance is tuned as intended (and spot any
-- drift between runs). Run via sqlcmd from run-benchmarks.{ps1,sh}; the output
-- is captured to results/sql-config.txt. Read-only; safe to run anytime.
SET NOCOUNT ON;

PRINT '=== Version / edition ===';
SELECT
    CAST(SERVERPROPERTY('ProductVersion') AS varchar(32)) AS product_version,
    CAST(SERVERPROPERTY('Edition')        AS varchar(64)) AS edition,
    CAST(SERVERPROPERTY('ProductLevel')   AS varchar(16)) AS product_level;

PRINT '=== Host / scheduler / memory (sys.dm_os_sys_info) ===';
SELECT
    cpu_count,
    hyperthread_ratio,
    scheduler_count,
    physical_memory_kb / 1024      AS physical_mem_mb,
    committed_target_kb / 1024     AS committed_target_mb,
    virtual_machine_type_desc,
    sqlserver_start_time
FROM sys.dm_os_sys_info;

PRINT '=== Key sp_configure values (value_in_use) ===';
SELECT name, value_in_use
FROM sys.configurations
WHERE name IN (
    'max server memory (MB)',
    'min server memory (MB)',
    'max degree of parallelism',
    'cost threshold for parallelism',
    'affinity mask',
    'affinity64 mask',
    'affinity I/O mask',
    'optimize for ad hoc workloads',
    'priority boost',
    'lightweight pooling',
    'max worker threads'
)
ORDER BY name;

PRINT '=== Online schedulers and their CPU ids (validates PROCESS AFFINITY) ===';
SELECT
    COUNT(*)                                   AS online_schedulers,
    MIN(cpu_id)                                AS min_cpu_id,
    MAX(cpu_id)                                AS max_cpu_id
FROM sys.dm_os_schedulers
WHERE status = 'VISIBLE ONLINE' AND scheduler_id < 1048576;

PRINT '=== tempdb files: count, total size, sample path (validates relocation) ===';
SELECT
    type_desc,
    COUNT(*)                 AS files,
    SUM(size) * 8 / 1024     AS total_mb,
    MIN(physical_name)       AS sample_path
FROM tempdb.sys.database_files
GROUP BY type_desc;

PRINT '=== Durability / recovery for key databases ===';
SELECT name, delayed_durability_desc, recovery_model_desc
FROM sys.databases
WHERE name IN ('master', 'tempdb', 'PerfTest')
ORDER BY name;

PRINT '=== Active trace flags ===';
DBCC TRACESTATUS(-1) WITH NO_INFOMSGS;
