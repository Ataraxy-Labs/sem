use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use sem_core::parser::plugins::create_default_registry;

const SMALL_COMPONENT: &str = r#"<script>
    let count = $state(0);
    function increment() {
        count += 1;
    }
</script>

<button onclick={increment}>
    clicks: {count}
</button>
"#;

const MEDIUM_COMPONENT: &str = r#"<script lang="ts">
    import { onMount } from 'svelte';
    import Header from './Header.svelte';
    import Footer from './Footer.svelte';

    interface User {
        id: number;
        name: string;
        email: string;
        role: 'admin' | 'user';
    }

    let users: User[] = $state([]);
    let loading = $state(true);
    let error: string | null = $state(null);
    let searchTerm = $state('');
    let selectedRole = $state<'all' | 'admin' | 'user'>('all');

    const filteredUsers = $derived(
        users.filter(user => {
            const matchesSearch = user.name.toLowerCase().includes(searchTerm.toLowerCase())
                || user.email.toLowerCase().includes(searchTerm.toLowerCase());
            const matchesRole = selectedRole === 'all' || user.role === selectedRole;
            return matchesSearch && matchesRole;
        })
    );

    async function fetchUsers() {
        try {
            const response = await fetch('/api/users');
            if (!response.ok) throw new Error('Failed to fetch');
            users = await response.json();
        } catch (e) {
            error = e instanceof Error ? e.message : 'Unknown error';
        } finally {
            loading = false;
        }
    }

    async function deleteUser(id: number) {
        if (!confirm('Are you sure?')) return;
        await fetch(`/api/users/${id}`, { method: 'DELETE' });
        users = users.filter(u => u.id !== id);
    }

    function exportCSV() {
        const csv = filteredUsers
            .map(u => `${u.id},${u.name},${u.email},${u.role}`)
            .join('\n');
        const blob = new Blob([csv], { type: 'text/csv' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = 'users.csv';
        a.click();
    }

    onMount(fetchUsers);
</script>

<Header title="User Management" />

<div class="container">
    <div class="controls">
        <input
            type="text"
            placeholder="Search users..."
            bind:value={searchTerm}
        />
        <select bind:value={selectedRole}>
            <option value="all">All roles</option>
            <option value="admin">Admin</option>
            <option value="user">User</option>
        </select>
        <button onclick={exportCSV}>Export CSV</button>
    </div>

    {#if loading}
        <div class="spinner">Loading...</div>
    {:else if error}
        <div class="error">
            <p>{error}</p>
            <button onclick={fetchUsers}>Retry</button>
        </div>
    {:else if filteredUsers.length === 0}
        <p class="empty">No users found.</p>
    {:else}
        <table>
            <thead>
                <tr>
                    <th>ID</th>
                    <th>Name</th>
                    <th>Email</th>
                    <th>Role</th>
                    <th>Actions</th>
                </tr>
            </thead>
            <tbody>
                {#each filteredUsers as user (user.id)}
                    <tr>
                        <td>{user.id}</td>
                        <td>{user.name}</td>
                        <td>{user.email}</td>
                        <td>
                            <span class="badge badge-{user.role}">
                                {user.role}
                            </span>
                        </td>
                        <td>
                            <button onclick={() => deleteUser(user.id)}>
                                Delete
                            </button>
                        </td>
                    </tr>
                {/each}
            </tbody>
        </table>
    {/if}

    <p class="count">{filteredUsers.length} of {users.length} users</p>
</div>

<Footer />

<style>
    .container {
        max-width: 800px;
        margin: 0 auto;
        padding: 1rem;
    }
    .controls {
        display: flex;
        gap: 1rem;
        margin-bottom: 1rem;
    }
    .controls input {
        flex: 1;
        padding: 0.5rem;
    }
    table {
        width: 100%;
        border-collapse: collapse;
    }
    th, td {
        padding: 0.5rem;
        border: 1px solid #ddd;
        text-align: left;
    }
    .badge {
        padding: 0.25rem 0.5rem;
        border-radius: 4px;
        font-size: 0.875rem;
    }
    .badge-admin { background: #fee2e2; color: #991b1b; }
    .badge-user { background: #dbeafe; color: #1e40af; }
    .spinner { text-align: center; padding: 2rem; }
    .error { color: red; text-align: center; }
    .empty { text-align: center; color: #666; }
    .count { text-align: right; color: #999; font-size: 0.875rem; }
</style>
"#;

const LARGE_COMPONENT: &str = r#"<script lang="ts" context="module">
    export interface Column<T> {
        key: keyof T;
        label: string;
        sortable?: boolean;
        render?: (value: T[keyof T], row: T) => string;
    }

    export type SortDirection = 'asc' | 'desc' | null;
</script>

<script lang="ts" generics="T extends Record<string, unknown>">
    import { tick } from 'svelte';

    interface Props {
        data: T[];
        columns: Column<T>[];
        pageSize?: number;
        selectable?: boolean;
        onselect?: (selected: T[]) => void;
    }

    let {
        data,
        columns,
        pageSize = 20,
        selectable = false,
        onselect
    }: Props = $props();

    let currentPage = $state(1);
    let sortKey = $state<keyof T | null>(null);
    let sortDirection = $state<SortDirection>(null);
    let selected = $state<Set<number>>(new Set());
    let expandedRows = $state<Set<number>>(new Set());
    let filterValues = $state<Record<string, string>>({});

    const filteredData = $derived.by(() => {
        let result = [...data];
        for (const [key, value] of Object.entries(filterValues)) {
            if (value) {
                result = result.filter(row =>
                    String(row[key]).toLowerCase().includes(value.toLowerCase())
                );
            }
        }
        return result;
    });

    const sortedData = $derived.by(() => {
        if (!sortKey || !sortDirection) return filteredData;
        return [...filteredData].sort((a, b) => {
            const aVal = a[sortKey!];
            const bVal = b[sortKey!];
            const cmp = aVal < bVal ? -1 : aVal > bVal ? 1 : 0;
            return sortDirection === 'asc' ? cmp : -cmp;
        });
    });

    const totalPages = $derived(Math.ceil(sortedData.length / pageSize));
    const paginatedData = $derived(
        sortedData.slice((currentPage - 1) * pageSize, currentPage * pageSize)
    );
    const allSelected = $derived(
        paginatedData.length > 0 && paginatedData.every((_, i) => selected.has(i))
    );

    function toggleSort(key: keyof T) {
        if (sortKey === key) {
            sortDirection = sortDirection === 'asc' ? 'desc' : sortDirection === 'desc' ? null : 'asc';
            if (!sortDirection) sortKey = null;
        } else {
            sortKey = key;
            sortDirection = 'asc';
        }
        currentPage = 1;
    }

    function toggleRow(index: number) {
        const next = new Set(selected);
        if (next.has(index)) next.delete(index);
        else next.add(index);
        selected = next;
        onselect?.(paginatedData.filter((_, i) => selected.has(i)));
    }

    function toggleAll() {
        if (allSelected) {
            selected = new Set();
        } else {
            selected = new Set(paginatedData.map((_, i) => i));
        }
        onselect?.(paginatedData.filter((_, i) => selected.has(i)));
    }

    function toggleExpand(index: number) {
        const next = new Set(expandedRows);
        if (next.has(index)) next.delete(index);
        else next.add(index);
        expandedRows = next;
    }

    async function goToPage(page: number) {
        currentPage = Math.max(1, Math.min(page, totalPages));
        selected = new Set();
        await tick();
    }

    function handleKeydown(e: KeyboardEvent, index: number) {
        if (e.key === 'Enter' || e.key === ' ') {
            e.preventDefault();
            toggleExpand(index);
        }
    }

    {#snippet sortIcon(key: keyof T)}
        {#if sortKey === key}
            <span class="sort-icon">{sortDirection === 'asc' ? '↑' : '↓'}</span>
        {/if}
    {/snippet}

    {#snippet pagination()}
        <nav class="pagination" aria-label="Table pagination">
            <button
                onclick={() => goToPage(1)}
                disabled={currentPage === 1}
            >
                First
            </button>
            <button
                onclick={() => goToPage(currentPage - 1)}
                disabled={currentPage === 1}
            >
                Prev
            </button>
            <span class="page-info">
                Page {currentPage} of {totalPages}
                ({sortedData.length} total)
            </span>
            <button
                onclick={() => goToPage(currentPage + 1)}
                disabled={currentPage === totalPages}
            >
                Next
            </button>
            <button
                onclick={() => goToPage(totalPages)}
                disabled={currentPage === totalPages}
            >
                Last
            </button>
        </nav>
    {/snippet}
</script>

<div class="data-table" role="grid">
    {@render pagination()}

    <div class="filters">
        {#each columns as col}
            <input
                type="text"
                placeholder="Filter {col.label}..."
                value={filterValues[col.key as string] ?? ''}
                oninput={(e) => {
                    filterValues[col.key as string] = e.currentTarget.value;
                    currentPage = 1;
                }}
            />
        {/each}
    </div>

    <table>
        <thead>
            <tr>
                {#if selectable}
                    <th class="checkbox-col">
                        <input
                            type="checkbox"
                            checked={allSelected}
                            onchange={toggleAll}
                            aria-label="Select all"
                        />
                    </th>
                {/if}
                {#each columns as col}
                    <th
                        class:sortable={col.sortable}
                        onclick={() => col.sortable && toggleSort(col.key)}
                        role={col.sortable ? 'columnheader button' : 'columnheader'}
                    >
                        {col.label}
                        {#if col.sortable}
                            {@render sortIcon(col.key)}
                        {/if}
                    </th>
                {/each}
                <th class="expand-col"></th>
            </tr>
        </thead>
        <tbody>
            {#each paginatedData as row, i (row)}
                <tr
                    class:selected={selected.has(i)}
                    class:expanded={expandedRows.has(i)}
                >
                    {#if selectable}
                        <td class="checkbox-col">
                            <input
                                type="checkbox"
                                checked={selected.has(i)}
                                onchange={() => toggleRow(i)}
                            />
                        </td>
                    {/if}
                    {#each columns as col}
                        <td>
                            {#if col.render}
                                {@html col.render(row[col.key], row)}
                            {:else}
                                {row[col.key]}
                            {/if}
                        </td>
                    {/each}
                    <td class="expand-col">
                        <button
                            class="expand-btn"
                            onclick={() => toggleExpand(i)}
                            onkeydown={(e) => handleKeydown(e, i)}
                            aria-expanded={expandedRows.has(i)}
                        >
                            {expandedRows.has(i) ? '−' : '+'}
                        </button>
                    </td>
                </tr>
                {#if expandedRows.has(i)}
                    <tr class="detail-row">
                        <td colspan={columns.length + (selectable ? 2 : 1)}>
                            <pre>{JSON.stringify(row, null, 2)}</pre>
                        </td>
                    </tr>
                {/if}
            {/each}
        </tbody>
    </table>

    {@render pagination()}
</div>

<style>
    .data-table {
        font-family: system-ui, sans-serif;
        border: 1px solid #e2e8f0;
        border-radius: 8px;
        overflow: hidden;
    }
    .filters {
        display: flex;
        gap: 0.5rem;
        padding: 0.75rem;
        background: #f8fafc;
        border-bottom: 1px solid #e2e8f0;
    }
    .filters input {
        flex: 1;
        padding: 0.375rem 0.5rem;
        border: 1px solid #cbd5e1;
        border-radius: 4px;
        font-size: 0.8125rem;
    }
    table { width: 100%; border-collapse: collapse; }
    th, td { padding: 0.625rem 0.75rem; text-align: left; border-bottom: 1px solid #e2e8f0; }
    th { background: #f1f5f9; font-weight: 600; font-size: 0.8125rem; text-transform: uppercase; letter-spacing: 0.05em; color: #475569; }
    th.sortable { cursor: pointer; user-select: none; }
    th.sortable:hover { background: #e2e8f0; }
    .sort-icon { margin-left: 0.25rem; }
    .checkbox-col { width: 2.5rem; text-align: center; }
    .expand-col { width: 2.5rem; text-align: center; }
    .expand-btn { background: none; border: 1px solid #cbd5e1; border-radius: 4px; cursor: pointer; width: 1.5rem; height: 1.5rem; display: flex; align-items: center; justify-content: center; }
    tr.selected { background: #eff6ff; }
    tr.expanded { background: #f0fdf4; }
    .detail-row td { background: #fafafa; padding: 1rem; }
    .detail-row pre { margin: 0; font-size: 0.8125rem; white-space: pre-wrap; }
    .pagination { display: flex; align-items: center; justify-content: center; gap: 0.5rem; padding: 0.75rem; background: #f8fafc; border-bottom: 1px solid #e2e8f0; }
    .pagination button { padding: 0.375rem 0.75rem; border: 1px solid #cbd5e1; border-radius: 4px; background: white; cursor: pointer; }
    .pagination button:disabled { opacity: 0.5; cursor: default; }
    .pagination button:not(:disabled):hover { background: #f1f5f9; }
    .page-info { font-size: 0.875rem; color: #64748b; }
</style>
"#;

const MODULE_FILE: &str = r#"import { writable, derived } from 'svelte/store';

export interface AppState {
    theme: 'light' | 'dark';
    locale: string;
    sidebarOpen: boolean;
}

export const appState = writable<AppState>({
    theme: 'light',
    locale: 'en',
    sidebarOpen: true,
});

export const isDark = derived(appState, ($state) => $state.theme === 'dark');

export function toggleTheme() {
    appState.update(s => ({ ...s, theme: s.theme === 'light' ? 'dark' : 'light' }));
}

export function setLocale(locale: string) {
    appState.update(s => ({ ...s, locale }));
}

export function toggleSidebar() {
    appState.update(s => ({ ...s, sidebarOpen: !s.sidebarOpen }));
}
"#;

fn bench_parse(c: &mut Criterion) {
    let registry = create_default_registry();
    let plugin = registry.get_plugin("App.svelte").unwrap();
    let module_plugin = registry.get_plugin("state.svelte.ts").unwrap();

    let mut group = c.benchmark_group("svelte_parse");

    group.bench_function("small_component", |b| {
        b.iter(|| plugin.extract_entities(black_box(SMALL_COMPONENT), "App.svelte"))
    });

    group.bench_function("medium_component", |b| {
        b.iter(|| plugin.extract_entities(black_box(MEDIUM_COMPONENT), "UserList.svelte"))
    });

    group.bench_function("large_component", |b| {
        b.iter(|| plugin.extract_entities(black_box(LARGE_COMPONENT), "DataTable.svelte"))
    });

    group.bench_function("module_file", |b| {
        b.iter(|| module_plugin.extract_entities(black_box(MODULE_FILE), "state.svelte.ts"))
    });

    group.finish();
}

fn bench_diff(c: &mut Criterion) {
    use sem_core::git::types::{FileChange, FileStatus};
    use sem_core::parser::differ::compute_semantic_diff;

    let registry = create_default_registry();

    // Simulate a small edit: add a function
    let before = MEDIUM_COMPONENT;
    let after = MEDIUM_COMPONENT.replace(
        "onMount(fetchUsers);",
        "function resetFilters() {\n        searchTerm = '';\n        selectedRole = 'all';\n    }\n\n    onMount(fetchUsers);",
    );

    let changes = vec![FileChange {
        file_path: "UserList.svelte".to_string(),
        status: FileStatus::Modified,
        old_file_path: None,
        before_content: Some(before.to_string()),
        after_content: Some(after.clone()),
    }];

    let mut group = c.benchmark_group("svelte_diff");

    group.bench_function("medium_component_small_edit", |b| {
        b.iter(|| compute_semantic_diff(black_box(&changes), &registry, None, None))
    });

    // Structural change: add a new control flow block
    let before2 = MEDIUM_COMPONENT;
    let after2 = MEDIUM_COMPONENT.replace(
        "<Footer />",
        "{#if users.length > 100}\n        <p class=\"warning\">Large dataset - consider filtering</p>\n    {/if}\n\n    <Footer />",
    );

    let changes2 = vec![FileChange {
        file_path: "UserList.svelte".to_string(),
        status: FileStatus::Modified,
        old_file_path: None,
        before_content: Some(before2.to_string()),
        after_content: Some(after2),
    }];

    group.bench_function("medium_component_structural_change", |b| {
        b.iter(|| compute_semantic_diff(black_box(&changes2), &registry, None, None))
    });

    // Multi-file diff
    let changes3 = vec![
        FileChange {
            file_path: "UserList.svelte".to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(before.to_string()),
            after_content: Some(after),
        },
        FileChange {
            file_path: "DataTable.svelte".to_string(),
            status: FileStatus::Added,
            old_file_path: None,
            before_content: None,
            after_content: Some(LARGE_COMPONENT.to_string()),
        },
        FileChange {
            file_path: "state.svelte.ts".to_string(),
            status: FileStatus::Modified,
            old_file_path: None,
            before_content: Some(MODULE_FILE.to_string()),
            after_content: Some(MODULE_FILE.replace(
                "export function toggleSidebar",
                "export function closeSidebar() {\n    appState.update(s => ({ ...s, sidebarOpen: false }));\n}\n\nexport function toggleSidebar",
            )),
        },
    ];

    group.bench_function("multi_file_diff", |b| {
        b.iter(|| compute_semantic_diff(black_box(&changes3), &registry, None, None))
    });

    group.finish();
}

criterion_group!(benches, bench_parse, bench_diff);
criterion_main!(benches);
