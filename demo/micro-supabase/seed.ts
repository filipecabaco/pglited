import postgres from "npm:postgres@3.4.5";

const PORT = Deno.args[0] || "54321";

const sql = postgres({
  host: "127.0.0.1",
  port: Number(PORT),
  user: "postgres",
  password: "password",
  database: "template1",
  ssl: false,
});

console.log("Seeding database for micro-supabase demo...");

// Extensions required by Supabase Auth
console.log("Creating extensions...");
await sql.unsafe(`CREATE EXTENSION IF NOT EXISTS "uuid-ossp"`);
await sql.unsafe(`CREATE EXTENSION IF NOT EXISTS "pgcrypto"`);

// Auth schema required by Supabase Auth (migrations run there)
console.log("Creating auth schema...");
await sql`CREATE SCHEMA IF NOT EXISTS auth`;

// Pre-create types in auth schema that migrations expect but create without namespace prefix
// This fixes a bug where some migrations use "create type foo" instead of "create type auth.foo"
// but later migrations try to "alter type auth.foo"
console.log("Pre-creating auth types...");
await sql.unsafe(`
  DO $$ BEGIN
    CREATE TYPE auth.factor_type AS ENUM('totp', 'webauthn');
  EXCEPTION WHEN duplicate_object THEN NULL; END $$
`);
await sql.unsafe(`
  DO $$ BEGIN
    CREATE TYPE auth.factor_status AS ENUM('unverified', 'verified');
  EXCEPTION WHEN duplicate_object THEN NULL; END $$
`);
await sql.unsafe(`
  DO $$ BEGIN
    CREATE TYPE auth.aal_level AS ENUM('aal1', 'aal2', 'aal3');
  EXCEPTION WHEN duplicate_object THEN NULL; END $$
`);
await sql.unsafe(`
  DO $$ BEGIN
    CREATE TYPE auth.code_challenge_method AS ENUM('s256', 'plain');
  EXCEPTION WHEN duplicate_object THEN NULL; END $$
`);
await sql.unsafe(`
  DO $$ BEGIN
    CREATE TYPE auth.one_time_token_type AS ENUM(
      'confirmation_token',
      'reauthentication_token',
      'recovery_token',
      'email_change_token_new',
      'email_change_token_current',
      'phone_change_token'
    );
  EXCEPTION WHEN duplicate_object THEN NULL; END $$
`);

// API schema for PostgREST
console.log("Creating api schema...");
await sql`CREATE SCHEMA IF NOT EXISTS api`;

await sql`
  CREATE TABLE IF NOT EXISTS api.products (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    category TEXT NOT NULL,
    price NUMERIC(10,2) NOT NULL,
    in_stock BOOLEAN DEFAULT true,
    created_at TIMESTAMPTZ DEFAULT now()
  )
`;

await sql`TRUNCATE api.products RESTART IDENTITY`;

await sql`
  INSERT INTO api.products (name, category, price, in_stock) VALUES
    ('Espresso',        'coffee', 3.50, true),
    ('Cappuccino',      'coffee', 4.75, true),
    ('Cold Brew',       'coffee', 5.00, true),
    ('Matcha Latte',    'tea',    5.50, true),
    ('Earl Grey',       'tea',    3.00, true),
    ('Chai Latte',      'tea',    4.25, true),
    ('Croissant',       'pastry', 3.25, false),
    ('Blueberry Muffin','pastry', 3.75, true),
    ('Sourdough Loaf',  'bakery', 7.00, true),
    ('Bagel',           'bakery', 2.50, true)
`;

// Roles for PostgREST
console.log("Creating roles...");

await sql.unsafe(`
  DO $$ BEGIN
    CREATE ROLE web_anon NOLOGIN;
  EXCEPTION WHEN duplicate_object THEN NULL;
  END $$
`);

await sql.unsafe(`
  DO $$ BEGIN
    CREATE ROLE authenticated NOLOGIN;
  EXCEPTION WHEN duplicate_object THEN NULL;
  END $$
`);

await sql.unsafe(`
  DO $$ BEGIN
    CREATE ROLE authenticator NOINHERIT LOGIN PASSWORD 'password';
  EXCEPTION WHEN duplicate_object THEN NULL;
  END $$
`);

await sql`GRANT web_anon TO authenticator`;
await sql`GRANT authenticated TO authenticator`;

// Permissions
console.log("Granting permissions...");

// web_anon: read-only access to api schema
await sql`GRANT USAGE ON SCHEMA api TO web_anon`;
await sql`GRANT SELECT ON ALL TABLES IN SCHEMA api TO web_anon`;

// authenticated: full CRUD on api schema
await sql`GRANT USAGE ON SCHEMA api TO authenticated`;
await sql`GRANT ALL ON ALL TABLES IN SCHEMA api TO authenticated`;
await sql`GRANT USAGE ON ALL SEQUENCES IN SCHEMA api TO authenticated`;

const rows = await sql`SELECT count(*) AS total FROM api.products`;
console.log(`Seeded ${rows[0].total} products`);
console.log("Database ready for Auth + PostgREST");

await sql.end();
