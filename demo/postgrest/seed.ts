import postgres from "npm:postgres@3.4.5";

const PORT = Deno.args[0] || "5432";

const sql = postgres({
  host: "127.0.0.1",
  port: Number(PORT),
  user: "postgres",
  password: "password",
  database: "template1",
  ssl: false,
});

console.log("Seeding database...");

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
    ('Espresso',       'coffee',  3.50,  true),
    ('Cappuccino',     'coffee',  4.75,  true),
    ('Cold Brew',      'coffee',  5.00,  true),
    ('Matcha Latte',   'tea',     5.50,  true),
    ('Earl Grey',      'tea',     3.00,  true),
    ('Chai Latte',     'tea',     4.25,  true),
    ('Croissant',      'pastry',  3.25,  false),
    ('Blueberry Muffin','pastry', 3.75,  true),
    ('Sourdough Loaf', 'bakery',  7.00,  true),
    ('Bagel',          'bakery',  2.50,  true)
`;

await sql.unsafe(`
  DO $$ BEGIN
    CREATE ROLE web_anon NOLOGIN;
  EXCEPTION WHEN duplicate_object THEN NULL;
  END $$
`);
await sql`GRANT USAGE ON SCHEMA api TO web_anon`;
await sql`GRANT SELECT ON ALL TABLES IN SCHEMA api TO web_anon`;

await sql.unsafe(`
  DO $$ BEGIN
    CREATE ROLE authenticator NOINHERIT LOGIN PASSWORD 'password';
  EXCEPTION WHEN duplicate_object THEN NULL;
  END $$
`);
await sql`GRANT web_anon TO authenticator`;

const rows = await sql`SELECT count(*) AS total FROM api.products`;
console.log(`Seeded ${rows[0].total} products`);

await sql.end();
