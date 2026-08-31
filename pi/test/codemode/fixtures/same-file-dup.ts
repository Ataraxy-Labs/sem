// Two entities named `widget`, in the SAME file (unlike dup-a.ts/dup-b.ts,
// which put one `widget` in each of two different files). Mirrors this
// repo's own real "execute" case: extensions/pi-sem.ts defines `execute`
// twice, so even after impact()'s resolveOneFile() disambiguates to ONE
// file, sem impact can still refuse ambiguity WITHIN that file.
export class Alpha {
  widget(): number {
    return 1;
  }
}

export class Beta {
  widget(): number {
    return 2;
  }
}
