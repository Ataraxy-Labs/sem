import React from "react";

export default function Home() {
  return (
    <div className="min-h-screen">
      {/* Hero */}
      <section className="px-6 pt-20 pb-16 max-w-5xl mx-auto">
        <p className="text-sm tracking-widest text-gray-500 uppercase mb-4" style={{ fontFamily: "var(--font-heading)" }}>
          by <a href="https://ataraxy-labs.com" className="hover:text-gray-300 transition-colors">Ataraxy Labs</a>
        </p>
        <h1 className="text-5xl md:text-7xl font-bold mb-6 leading-tight" style={{ fontFamily: "var(--font-heading)" }}>
          sem
        </h1>
        <p className="text-xl md:text-2xl text-gray-300 mb-4 max-w-3xl leading-relaxed">
          Semantic version control built on Git. Entity-level diff, blame, graph, and impact analysis.
        </p>
        <p className="text-lg text-gray-500 mb-10 max-w-2xl">
          Git tracks lines. Developers think in functions. sem bridges the gap with AST-normalized structural hashing across 11 languages.
        </p>

        <div className="flex flex-wrap gap-4 mb-16">
          <a
            href="https://github.com/Ataraxy-Labs/sem"
            target="_blank"
            rel="noopener noreferrer"
            className="px-6 py-3 bg-white text-black font-semibold rounded-lg hover:bg-gray-200 transition-colors text-sm"
            style={{ fontFamily: "var(--font-heading)" }}
          >
            GitHub
          </a>
          <a
            href="https://ataraxy-labs.com/blogs/structural-hashing-version-control"
            className="px-6 py-3 border border-white/20 rounded-lg hover:border-white/40 transition-colors text-sm"
            style={{ fontFamily: "var(--font-heading)" }}
          >
            Read the Technical Deep Dive
          </a>
        </div>

        {/* Install */}
        <div className="mb-16">
          <pre><code>cd sem/crates && cargo build --release</code></pre>
        </div>
      </section>

      {/* Core Concept */}
      <section className="px-6 py-16 border-t border-white/10">
        <div className="max-w-5xl mx-auto">
          <h2 className="text-3xl font-bold mb-8" style={{ fontFamily: "var(--font-heading)" }}>
            Structural Hashing
          </h2>
          <p className="text-gray-400 mb-6 max-w-3xl leading-relaxed">
            Every entity gets an AST-normalized hash that ignores variable names, whitespace, and comments. Two functions with different formatting but identical behavior produce the same hash. This is the foundation for everything sem does.
          </p>
          <div className="grid md:grid-cols-2 gap-8">
            <div>
              <p className="text-sm text-gray-500 mb-2">Before reformatting</p>
              <pre><code>{`function total(items, tax) {
  const sub = items.reduce(
    (s, i) => s + i.price, 0
  );
  return sub * (1 + tax);
}`}</code></pre>
            </div>
            <div>
              <p className="text-sm text-gray-500 mb-2">After reformatting + renaming</p>
              <pre><code>{`function total(
    products, taxRate
) {
  const base = products.reduce(
    (acc, p) => acc + p.price,
    0
  );
  return base * (1 + taxRate);
}`}</code></pre>
            </div>
          </div>
          <p className="text-gray-500 text-sm mt-4">
            Same structural hash. sem knows this is a cosmetic change, not a behavioral one.
          </p>
        </div>
      </section>

      {/* Commands */}
      <section className="px-6 py-16 border-t border-white/10">
        <div className="max-w-5xl mx-auto">
          <h2 className="text-3xl font-bold mb-12" style={{ fontFamily: "var(--font-heading)" }}>
            Commands
          </h2>

          {/* sem diff */}
          <div className="mb-12">
            <h3 className="text-xl font-semibold mb-4" style={{ fontFamily: "var(--font-heading)" }}>sem diff</h3>
            <p className="text-gray-400 mb-4">Entity-level diff that classifies each change as structural or cosmetic.</p>
            <pre><code>{`$ sem diff HEAD~1

src/payment.rs
  Modified: process_payment
    Type: STRUCTURAL (hash changed: a3f2b1 -> c7d4e9)
    Lines: 45-62 -> 45-68 (+6 lines)

  Modified: validate_order
    Type: COSMETIC (hash unchanged: e8f1a2)
    Lines: 70-85 -> 74-89 (reformatted)

  Added: calculate_tax
    Lines: 90-102`}</code></pre>
          </div>

          {/* sem blame */}
          <div className="mb-12">
            <h3 className="text-xl font-semibold mb-4" style={{ fontFamily: "var(--font-heading)" }}>sem blame</h3>
            <p className="text-gray-400 mb-4">Entity-level blame that skips cosmetic-only commits. Shows who last made a structural change to each function.</p>
            <pre><code>{`$ sem blame src/payment.rs

Entity               Author     Commit    Date
──────────────────   ────────   ────────  ───────────
process_payment      alice      a3f2b1c   2026-01-15
validate_order       bob        e8f1a2d   2025-12-03
calculate_tax        alice      c7d4e9f   2026-02-01
PaymentError         charlie    b2c3d4e   2025-11-20`}</code></pre>
          </div>

          {/* sem graph */}
          <div className="mb-12">
            <h3 className="text-xl font-semibold mb-4" style={{ fontFamily: "var(--font-heading)" }}>sem graph</h3>
            <p className="text-gray-400 mb-4">Cross-file entity dependency graph. Name-based analysis that works across all 11 languages without a type checker.</p>
            <pre><code>{`$ sem graph src/payment.rs

process_payment
  -> validate_order (src/payment.rs)
  -> calculate_tax (src/payment.rs)
  -> charge_card (src/stripe.rs)
  -> log_transaction (src/logging.rs)

validate_order
  -> check_inventory (src/inventory.rs)
  -> verify_address (src/shipping.rs)`}</code></pre>
          </div>

          {/* sem impact */}
          <div className="mb-12">
            <h3 className="text-xl font-semibold mb-4" style={{ fontFamily: "var(--font-heading)" }}>sem impact</h3>
            <p className="text-gray-400 mb-4">Transitive impact analysis. Change a function, see everything that might break.</p>
            <pre><code>{`$ sem impact src/payment.rs::calculate_tax

Direct dependents (depth 1):
  process_payment (src/payment.rs)

Transitive dependents (depth 2):
  handle_checkout (src/api/checkout.rs)
  process_batch_payments (src/batch.rs)

Transitive dependents (depth 3):
  run_daily_billing (src/cron/billing.rs)

Total impact: 4 entities across 4 files`}</code></pre>
          </div>
        </div>
      </section>

      {/* Performance */}
      <section className="px-6 py-16 border-t border-white/10">
        <div className="max-w-5xl mx-auto">
          <h2 className="text-3xl font-bold mb-8" style={{ fontFamily: "var(--font-heading)" }}>
            Performance
          </h2>
          <p className="text-gray-400 mb-6">
            Rewritten from Node.js to Rust. ~10x faster on every operation.
          </p>
          <div className="grid md:grid-cols-4 gap-6">
            <div className="border border-white/10 rounded-lg p-6 text-center">
              <p className="text-3xl font-bold" style={{ fontFamily: "var(--font-heading)" }}>12ms</p>
              <p className="text-gray-500 text-sm mt-2">Entity extraction</p>
              <p className="text-gray-600 text-xs mt-1">500-line file</p>
            </div>
            <div className="border border-white/10 rounded-lg p-6 text-center">
              <p className="text-3xl font-bold" style={{ fontFamily: "var(--font-heading)" }}>28ms</p>
              <p className="text-gray-500 text-sm mt-2">Full diff</p>
              <p className="text-gray-600 text-xs mt-1">two 500-line files</p>
            </div>
            <div className="border border-white/10 rounded-lg p-6 text-center">
              <p className="text-3xl font-bold" style={{ fontFamily: "var(--font-heading)" }}>95ms</p>
              <p className="text-gray-500 text-sm mt-2">Graph build</p>
              <p className="text-gray-600 text-xs mt-1">50-file project</p>
            </div>
            <div className="border border-white/10 rounded-lg p-6 text-center">
              <p className="text-3xl font-bold" style={{ fontFamily: "var(--font-heading)" }}>180ms</p>
              <p className="text-gray-500 text-sm mt-2">Full blame</p>
              <p className="text-gray-600 text-xs mt-1">100-commit history</p>
            </div>
          </div>
        </div>
      </section>

      {/* Languages */}
      <section className="px-6 py-16 border-t border-white/10">
        <div className="max-w-5xl mx-auto">
          <h2 className="text-3xl font-bold mb-8" style={{ fontFamily: "var(--font-heading)" }}>
            11 Languages
          </h2>
          <div className="flex flex-wrap gap-3">
            {["TypeScript", "TSX", "JavaScript", "Python", "Go", "Rust", "Java", "C", "C++", "Ruby", "C#"].map((lang) => (
              <span key={lang} className="px-4 py-2 border border-white/15 rounded-lg text-sm text-gray-300">
                {lang}
              </span>
            ))}
          </div>
          <p className="text-gray-500 text-sm mt-6">
            Each language has a parser plugin with language-specific normalization for structural hashing. Tree-sitter grammars compiled into the binary -- no runtime dependencies.
          </p>
        </div>
      </section>

      {/* 3-Phase Matching */}
      <section className="px-6 py-16 border-t border-white/10">
        <div className="max-w-5xl mx-auto">
          <h2 className="text-3xl font-bold mb-8" style={{ fontFamily: "var(--font-heading)" }}>
            3-Phase Entity Matching
          </h2>
          <p className="text-gray-400 mb-8">
            When comparing entities between versions, sem uses a 3-phase matching algorithm inspired by IntelliMerge:
          </p>
          <div className="space-y-6">
            <div className="border-l-2 border-white/20 pl-6">
              <h3 className="text-lg font-semibold mb-2" style={{ fontFamily: "var(--font-heading)" }}>Phase 1: Exact Name Match</h3>
              <p className="text-gray-400 text-sm">Entities with the same name and kind are matched directly. Handles the common case.</p>
            </div>
            <div className="border-l-2 border-white/20 pl-6">
              <h3 className="text-lg font-semibold mb-2" style={{ fontFamily: "var(--font-heading)" }}>Phase 2: Structural Hash Match</h3>
              <p className="text-gray-400 text-sm">Among unmatched entities, pairs with the same structural hash are detected as renames. The body is identical, only the name changed.</p>
            </div>
            <div className="border-l-2 border-white/20 pl-6">
              <h3 className="text-lg font-semibold mb-2" style={{ fontFamily: "var(--font-heading)" }}>Phase 3: Similarity Match</h3>
              <p className="text-gray-400 text-sm">Remaining entities are compared by partial AST overlap. Similarity above 0.7 indicates a modified rename.</p>
            </div>
          </div>
        </div>
      </section>

      {/* Companion tool */}
      <section className="px-6 py-16 border-t border-white/10">
        <div className="max-w-5xl mx-auto">
          <h2 className="text-3xl font-bold mb-4" style={{ fontFamily: "var(--font-heading)" }}>
            Works with Weave
          </h2>
          <p className="text-gray-400 mb-6 max-w-3xl leading-relaxed">
            sem and <a href="/weave" className="text-white underline hover:text-gray-300">Weave</a> are complementary tools built on the same foundation: sem-core&apos;s entity extraction and structural hashing.
          </p>
          <div className="grid md:grid-cols-2 gap-8">
            <div className="border border-white/10 rounded-lg p-6">
              <h3 className="text-base font-semibold mb-2" style={{ fontFamily: "var(--font-heading)" }}>sem</h3>
              <p className="text-gray-400 text-sm">Understand code history. What changed, who changed it, what depends on it, what might break.</p>
            </div>
            <div className="border border-white/10 rounded-lg p-6">
              <h3 className="text-base font-semibold mb-2" style={{ fontFamily: "var(--font-heading)" }}>Weave</h3>
              <p className="text-gray-400 text-sm">Multi-developer coordination. Can these changes merge cleanly, who is editing what, where are the conflicts.</p>
            </div>
          </div>
        </div>
      </section>

      {/* Footer */}
      <footer className="px-6 py-12 border-t border-white/10 text-center text-gray-600 text-sm">
        <p>MIT License. Built by <a href="https://ataraxy-labs.com" className="text-gray-400 hover:text-white transition-colors">Ataraxy Labs</a>.</p>
      </footer>
    </div>
  );
}
