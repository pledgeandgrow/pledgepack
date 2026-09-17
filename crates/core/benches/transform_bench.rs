// PRODUCTION-READINESS-100.md goal 98: criterion benchmarks for the core
// transform pipeline, tracked over time in CI (non-gating — see
// `.github/workflows/ci.yml`'s `bench` job). Deliberately scoped to
// `transform::transform()` across the three module kinds a real app spends
// the most build time in (TSX, TypeScript, CSS) rather than trying to cover
// every module kind or the optimizer's tree-shaking pass in this first pass
// — those are real gaps, not silently dropped (see this goal's entry in
// PRODUCTION-READINESS-100.md for the scope note).

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use pledgepack_core::config::PledgeConfig;
use pledgepack_core::module::ModuleKind;
use pledgepack_core::transform::transform;

const SMALL_TSX: &str = r#"
import { useState } from "react";

export function Counter() {
  const [count, setCount] = useState(0);
  return (
    <button onClick={() => setCount(count + 1)}>
      Clicked {count} times
    </button>
  );
}
"#;

const MEDIUM_TS: &str = r#"
interface User {
  id: number;
  name: string;
  email: string;
  roles: string[];
}

class UserRepository {
  private users: Map<number, User> = new Map();

  add(user: User): void {
    this.users.set(user.id, user);
  }

  findById(id: number): User | undefined {
    return this.users.get(id);
  }

  findByRole(role: string): User[] {
    return Array.from(this.users.values()).filter((u) =>
      u.roles.includes(role)
    );
  }

  update(id: number, patch: Partial<User>): User | undefined {
    const existing = this.users.get(id);
    if (!existing) return undefined;
    const updated = { ...existing, ...patch };
    this.users.set(id, updated);
    return updated;
  }

  delete(id: number): boolean {
    return this.users.delete(id);
  }

  count(): number {
    return this.users.size;
  }
}

export function createRepository(initial: User[] = []): UserRepository {
  const repo = new UserRepository();
  for (const user of initial) {
    repo.add(user);
  }
  return repo;
}
"#;

const MEDIUM_CSS: &str = r#"
.card {
  display: flex;
  flex-direction: column;
  padding: 1rem;
  border-radius: 0.5rem;
  background: var(--card-bg, #fff);
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.1);
}

.card:hover {
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.15);
}

.card__title {
  font-size: 1.25rem;
  font-weight: 600;
  margin-bottom: 0.5rem;
}

.card__body {
  color: #444;
  line-height: 1.5;
}

@media (max-width: 768px) {
  .card {
    padding: 0.75rem;
  }
}

.dark .card {
  background: var(--card-bg-dark, #222);
  color: #eee;
}
"#;

fn bench_transform_tsx(c: &mut Criterion) {
    let config = PledgeConfig::default();
    c.bench_with_input(
        BenchmarkId::new("transform", "small_tsx"),
        &SMALL_TSX,
        |b, source| {
            b.iter(|| transform(source, ModuleKind::Tsx, "Counter.tsx", false, &config).unwrap());
        },
    );
}

fn bench_transform_ts(c: &mut Criterion) {
    let config = PledgeConfig::default();
    c.bench_with_input(
        BenchmarkId::new("transform", "medium_ts"),
        &MEDIUM_TS,
        |b, source| {
            b.iter(|| {
                transform(
                    source,
                    ModuleKind::TypeScript,
                    "repository.ts",
                    false,
                    &config,
                )
                .unwrap()
            });
        },
    );
}

fn bench_transform_css(c: &mut Criterion) {
    let config = PledgeConfig::default();
    c.bench_with_input(
        BenchmarkId::new("transform", "medium_css"),
        &MEDIUM_CSS,
        |b, source| {
            b.iter(|| transform(source, ModuleKind::Css, "card.css", false, &config).unwrap());
        },
    );
}

fn bench_transform_tsx_production(c: &mut Criterion) {
    let config = PledgeConfig::default();
    c.bench_with_input(
        BenchmarkId::new("transform_production", "small_tsx"),
        &SMALL_TSX,
        |b, source| {
            b.iter(|| transform(source, ModuleKind::Tsx, "Counter.tsx", true, &config).unwrap());
        },
    );
}

criterion_group!(
    benches,
    bench_transform_tsx,
    bench_transform_ts,
    bench_transform_css,
    bench_transform_tsx_production
);
criterion_main!(benches);
