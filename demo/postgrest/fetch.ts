const POSTGREST_URL = Deno.args[0] || "http://localhost:3000";

async function query(path: string) {
  const res = await fetch(`${POSTGREST_URL}${path}`);
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return res.json();
}

console.log("=== All products ===");
const all = await query("/products");
console.table(all);

console.log("\n=== Coffee only (category=eq.coffee) ===");
const coffee = await query("/products?category=eq.coffee");
console.table(coffee);

console.log("\n=== In stock, ordered by price desc ===");
const sorted = await query("/products?in_stock=is.true&order=price.desc");
console.table(sorted);

console.log("\n=== Cheap items (price < 4.00) ===");
const cheap = await query("/products?price=lt.4.00&order=price.asc");
console.table(cheap);

console.log("\n=== Count via HEAD + Content-Range ===");
const head = await fetch(`${POSTGREST_URL}/products`, {
  method: "HEAD",
  headers: { Prefer: "count=exact" },
});
console.log("Content-Range:", head.headers.get("content-range"));
