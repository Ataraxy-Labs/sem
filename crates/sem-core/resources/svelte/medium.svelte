<script lang="ts">
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
