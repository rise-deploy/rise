// Access classes, ranked by who can reach an app in them.

export type AccessRequirement = 'None' | 'Authenticated' | 'Member';

export interface AccessClass {
    id: string;
    display_name: string;
    description?: string;
    /** Absent from older backends; such a class ranks as unknown (last). */
    access_requirement?: AccessRequirement;
}

const RANK: Record<AccessRequirement, number> = { Member: 0, Authenticated: 1, None: 2 };

/** 0 = most restrictive. Unknown requirements sort after the known ones. */
export function openness(c: AccessClass | undefined): number {
    return c?.access_requirement ? RANK[c.access_requirement] : 3;
}

/** Most restrictive first; ties keep the backend's order. */
export function sortByRestrictiveness(list: AccessClass[]): AccessClass[] {
    return list.map((c, i) => ({ c, i })).sort((a, b) => openness(a.c) - openness(b.c) || a.i - b.i).map(x => x.c);
}

/** The safest default for a new project. */
export function mostRestrictive(list: AccessClass[]): AccessClass | undefined {
    return sortByRestrictiveness(list)[0];
}

/**
 * Whether moving from `from` to `to` lets more people reach the app. Unknown
 * requirements count as widening, so the change is confirmed rather than not.
 */
export function widensAccess(from: AccessClass | undefined, to: AccessClass | undefined): boolean {
    if (!to?.access_requirement || !from?.access_requirement) return true;
    return RANK[to.access_requirement] > RANK[from.access_requirement];
}

/** Who can reach an app in this class, in a sentence fragment. */
export function audience(c: AccessClass | undefined): string {
    switch (c?.access_requirement) {
        case 'None': return 'anyone on the internet';
        case 'Authenticated': return 'anyone who can sign in to Rise';
        case 'Member': return 'only the project owner and its team members';
        default: return c?.description || 'a different audience';
    }
}
