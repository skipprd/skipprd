import { defineSkipprDocs } from '@skippr/vitepress-theme'

const sidebar = [
  { text: 'Home', link: '/' },
  {
    text: 'Getting started',
    items: [
      { text: 'Install', link: '/getting-started/install' },
      { text: 'Python', link: '/python' },
      { text: 'Snowflake', link: '/getting-started/quickstart-snowflake' },
      { text: 'PostgreSQL', link: '/getting-started/quickstart-postgres' },
      { text: 'BigQuery', link: '/getting-started/quickstart-bigquery' },
      { text: 'S3 to Athena', link: '/getting-started/quickstart' },
      { text: 'Pipeline flow', link: '/getting-started/how-it-works' },
    ],
  },
  {
    text: 'Engine',
    items: [
      { text: 'How Skipprd works', link: '/concepts/how-it-works' },
      { text: 'Schema', link: '/concepts/schema' },
      { text: 'CDC', link: '/cdc/' },
      { text: 'CDC guarantees', link: '/cdc/guarantees' },
      { text: 'Exactly-once', link: '/concepts/exactly-once' },
      { text: 'Datalake', link: '/concepts/datalake' },
      { text: 'Source landing', link: '/concepts/source-landing-semantics' },
    ],
  },
  {
    text: 'CLI',
    items: [
      { text: 'Overview', link: '/cli/overview' },
      { text: 'Skipprd discover', link: '/cli/discover' },
      { text: 'Skipprd schema', link: '/cli/schema' },
      { text: 'Skipprd sync', link: '/cli/sync' },
      { text: 'Skipprd query', link: '/cli/query' },
      { text: 'Skipprd doctor', link: '/cli/doctor' },
      { text: 'Skipprd df', link: '/cli/df' },
    ],
  },
  {
    text: 'Configure',
    collapsed: false,
    items: [
      { text: 'skippr.yml', link: '/configuration/skippr-yml' },
      { text: 'Overview', link: '/configuration/overview' },
      { text: 'Pipeline', link: '/configuration/pipeline' },
      { text: 'Input', link: '/configuration/input' },
      { text: 'Output', link: '/configuration/output' },
      { text: 'Transforms', link: '/configuration/transforms' },
      { text: 'WAL and buffering', link: '/configuration/buffering' },
      { text: 'Advanced', link: '/configuration/advanced' },
    ],
  },
  {
    text: 'Data sources',
    collapsed: false,
    items: [
      { text: 'Catalog', link: '/connectors/' },
      {
        text: 'Databases',
        collapsed: true,
        items: [
          { text: 'ClickHouse', link: '/connectors/inputs/clickhouse' },
          { text: 'DynamoDB', link: '/connectors/inputs/dynamodb' },
          { text: 'MongoDB', link: '/connectors/inputs/mongodb' },
          { text: 'MotherDuck', link: '/connectors/inputs/motherduck' },
          { text: 'MSSQL', link: '/connectors/inputs/mssql' },
          { text: 'MySQL', link: '/connectors/inputs/mysql' },
          { text: 'PostgreSQL', link: '/connectors/inputs/postgres' },
          { text: 'Redshift', link: '/connectors/inputs/redshift' },
          { text: 'Delta Lake', link: '/connectors/inputs/delta_lake' },
        ],
      },
      {
        text: 'Object stores',
        collapsed: true,
        items: [
          { text: 'Local file', link: '/connectors/inputs/file' },
          { text: 'S3', link: '/connectors/inputs/s3' },
          { text: 'SFTP', link: '/connectors/inputs/sftp' },
        ],
      },
      {
        text: 'Streaming',
        collapsed: true,
        items: [
          { text: 'Kafka', link: '/connectors/inputs/kafka' },
          { text: 'SQS', link: '/connectors/inputs/sqs' },
          { text: 'Kinesis', link: '/connectors/inputs/kinesis' },
          { text: 'AMQP', link: '/connectors/inputs/amqp' },
          { text: 'SNS', link: '/connectors/inputs/sns' },
          { text: 'EventBridge', link: '/connectors/inputs/eventbridge' },
          { text: 'MQTT', link: '/connectors/inputs/mqtt' },
          { text: 'WebSocket', link: '/connectors/inputs/websocket' },
        ],
      },
      {
        text: 'HTTP / Network',
        collapsed: true,
        items: [
          { text: 'HTTP client', link: '/connectors/inputs/http_client' },
          { text: 'HTTP server', link: '/connectors/inputs/http_server' },
          { text: 'OTLP', link: '/connectors/inputs/otlp' },
          { text: 'Socket', link: '/connectors/inputs/socket' },
          { text: 'StatsD', link: '/connectors/inputs/statsd' },
          { text: 'PCAP', link: '/connectors/inputs/pcap' },
        ],
      },
      {
        text: 'API / SaaS',
        collapsed: true,
        items: [
          { text: 'Google Analytics (GA4)', link: '/connectors/inputs/google_analytics' },
          { text: 'Apple Search Ads', link: '/connectors/inputs/apple_search_ads' },
          { text: 'Google Ads', link: '/connectors/inputs/google_ads' },
          { text: 'Meta Ads', link: '/connectors/inputs/meta_ads' },
          { text: 'Meta Instagram Ads', link: '/connectors/inputs/meta_instagram_ads' },
          { text: 'LinkedIn Ads', link: '/connectors/inputs/linkedin_ads' },
          { text: 'X Ads', link: '/connectors/inputs/x_ads' },
          { text: 'AdRoll Ads', link: '/connectors/inputs/adroll_ads' },
          { text: 'Stripe', link: '/connectors/inputs/stripe' },
          { text: 'Shopify Admin', link: '/connectors/inputs/shopify_admin' },
          { text: 'HubSpot CRM', link: '/connectors/inputs/hubspot_crm' },
          { text: 'Xero Accounting', link: '/connectors/inputs/xero_accounting' },
          { text: 'Revolut Business', link: '/connectors/inputs/revolut_business' },
          { text: 'SumUp', link: '/connectors/inputs/sumup' },
        ],
      },
      {
        text: 'Other',
        collapsed: true,
        items: [{ text: 'Stdin', link: '/connectors/inputs/stdin' }],
      },
    ],
  },
  {
    text: 'Data sinks',
    collapsed: false,
    items: [
      {
        text: 'Warehouses',
        collapsed: true,
        items: [
          { text: 'Athena', link: '/connectors/outputs/athena' },
          { text: 'BigQuery', link: '/connectors/outputs/bigquery' },
          { text: 'ClickHouse', link: '/connectors/outputs/clickhouse' },
          { text: 'Databricks', link: '/connectors/outputs/databricks' },
          { text: 'Iceberg', link: '/connectors/outputs/iceberg' },
          { text: 'MotherDuck', link: '/connectors/outputs/motherduck' },
          { text: 'Postgres', link: '/connectors/outputs/postgres' },
          { text: 'Redshift', link: '/connectors/outputs/redshift' },
          { text: 'Snowflake', link: '/connectors/outputs/snowflake' },
          { text: 'Synapse', link: '/connectors/outputs/synapse' },
        ],
      },
      {
        text: 'Cloud storage',
        collapsed: true,
        items: [
          { text: 'S3', link: '/connectors/outputs/s3' },
          { text: 'GCS', link: '/connectors/outputs/gcs' },
          { text: 'Azure Blob', link: '/connectors/outputs/azure_blob' },
          { text: 'SFTP', link: '/connectors/outputs/sftp' },
        ],
      },
      {
        text: 'Messaging / other',
        collapsed: true,
        items: [
          { text: 'AMQP', link: '/connectors/outputs/amqp' },
          { text: 'Local file', link: '/connectors/outputs/file' },
          { text: 'Stdout', link: '/connectors/outputs/stdout' },
        ],
      },
    ],
  },
  {
    text: 'Schema sinks',
    collapsed: false,
    items: [
      { text: 'Glue', link: '/connectors/schema_sinks/glue' },
      { text: 'Iceberg', link: '/connectors/schema_sinks/iceberg' },
    ],
  },
  {
    text: 'Operations',
    items: [
      { text: 'Troubleshooting', link: '/operations/troubleshooting' },
      { text: 'Logging', link: '/operations/logging' },
    ],
  },
]

export default defineSkipprDocs({
  name: 'Skipprd',
  hostname: 'elt.skippr.io',
  description:
    'Self-hosted ELT engine. Describe a source and a destination in skippr.yml, then run Skipprd discover, Skipprd schema, and Skipprd sync.',
  srcDir: 'docs',
  outDir: '.vitepress/dist',
  srcExclude: [
    'maintainers/**',
    'cli/query.md',
    'cli/sql-help.md',
    'sql/**',
    'query/**',
    'connectors/inputs/google_search_console.md',
    'connectors/inputs/bing_webmaster_tools.md',
    'license.md',
  ],
  vite: {
    ssr: {
      noExternal: ['@skippr/vitepress-theme'],
    },
  },
  nav: [
    { text: 'Discover', link: '/cli/discover' },
    { text: 'Schema', link: '/cli/schema' },
    { text: 'Sync', link: '/cli/sync' },
    { text: 'Cloud ELT', link: 'https://skippr.io/elt/' },
  ],
  sidebar: {
    '/': sidebar,
  },
  extraThemeConfig: {
    socialLinks: [
      { icon: 'github', link: 'https://github.com/skipprd/skipprd' },
    ],
    footer: {
      message: 'This site is source-available under PolyForm Shield 1.0.0',
      copyright: `Copyright © ${new Date().getFullYear()} Skippr Ltd`,
    },
  },
})
