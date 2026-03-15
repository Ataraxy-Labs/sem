<script lang="ts" context="module">
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
