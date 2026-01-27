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

console.log("Seeding database for Supabase migration demo...");

await sql`CREATE SCHEMA IF NOT EXISTS app`;

console.log("Creating products table...");
await sql`
  CREATE TABLE IF NOT EXISTS app.products (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    category TEXT NOT NULL,
    price NUMERIC(10,2) NOT NULL,
    in_stock BOOLEAN DEFAULT true,
    created_at TIMESTAMPTZ DEFAULT now()
  )
`;

console.log("Creating users table...");
await sql`
  CREATE TABLE IF NOT EXISTS app.users (
    id SERIAL PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT now()
  )
`;

console.log("Creating orders table...");
await sql`
  CREATE TABLE IF NOT EXISTS app.orders (
    id SERIAL PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES app.users(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'pending',
    total NUMERIC(10,2) NOT NULL,
    created_at TIMESTAMPTZ DEFAULT now()
  )
`;

console.log("Creating order_items table...");
await sql`
  CREATE TABLE IF NOT EXISTS app.order_items (
    id SERIAL PRIMARY KEY,
    order_id INTEGER NOT NULL REFERENCES app.orders(id) ON DELETE CASCADE,
    product_id INTEGER NOT NULL REFERENCES app.products(id) ON DELETE CASCADE,
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    price_at_time NUMERIC(10,2) NOT NULL,
    created_at TIMESTAMPTZ DEFAULT now()
  )
`;

console.log("Truncating existing data...");
await sql`TRUNCATE app.order_items RESTART IDENTITY CASCADE`;
await sql`TRUNCATE app.orders RESTART IDENTITY CASCADE`;
await sql`TRUNCATE app.users RESTART IDENTITY CASCADE`;
await sql`TRUNCATE app.products RESTART IDENTITY CASCADE`;

console.log("Inserting products...");
await sql`
  INSERT INTO app.products (name, category, price, in_stock) VALUES
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

console.log("Inserting users...");
await sql`
  INSERT INTO app.users (email, name) VALUES
    ('alice@example.com', 'Alice Johnson'),
    ('bob@example.com', 'Bob Smith'),
    ('charlie@example.com', 'Charlie Brown'),
    ('diana@example.com', 'Diana Prince'),
    ('eve@example.com', 'Eve Williams')
`;

console.log("Inserting orders...");
await sql`
  INSERT INTO app.orders (user_id, status, total) VALUES
    (1, 'completed', 8.25),
    (1, 'pending', 5.50),
    (2, 'completed', 4.75),
    (3, 'shipped', 9.50),
    (4, 'pending', 11.00),
    (5, 'completed', 6.50)
`;

console.log("Inserting order_items...");
await sql`
  INSERT INTO app.order_items (order_id, product_id, quantity, price_at_time) VALUES
    (1, 1, 2, 3.50),
    (1, 4, 1, 5.50),
    (2, 4, 1, 5.50),
    (3, 2, 1, 4.75),
    (4, 6, 2, 4.25),
    (4, 10, 1, 2.50),
    (5, 3, 2, 5.00),
    (5, 9, 1, 7.00),
    (6, 5, 1, 3.00),
    (6, 8, 1, 3.75)
`;

const productCount = await sql`SELECT count(*) AS total FROM app.products`;
const userCount = await sql`SELECT count(*) AS total FROM app.users`;
const orderCount = await sql`SELECT count(*) AS total FROM app.orders`;
const orderItemCount = await sql`SELECT count(*) AS total FROM app.order_items`;

console.log(`
✅ Seeding complete!
   Products: ${productCount[0].total}
   Users: ${userCount[0].total}
   Orders: ${orderCount[0].total}
   Order Items: ${orderItemCount[0].total}
`);

await sql.end();
