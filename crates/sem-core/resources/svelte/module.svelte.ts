import { writable, derived } from 'svelte/store';

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
