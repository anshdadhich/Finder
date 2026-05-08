using FastSearch.Mft;

namespace FastSearch.Index;

public sealed record SearchResult(string FullPath, string Name, byte Rank, bool IsDir);

public static class Searcher
{
    private static readonly string[] AppExtensions = ["exe", "lnk", "msi", "appx", "msix"];
    private static readonly string[] AppPathMarkers =
    [
        "\\program files\\", "\\program files (x86)\\",
        "\\start menu\\", "\\desktop\\", "\\appdata\\",
    ];

    public static List<SearchResult> Search(IndexStore store, string query, int limit, bool caseSensitive, IReadOnlyList<string> excludedDirs)
    {
        if (query.Length == 0) return [];
        var q = caseSensitive ? query : query.ToLowerInvariant();
        var entries = store.Entries;

        var candidates = entries
            .AsParallel()
            .Select((entry, idx) =>
            {
                var nameCmp = caseSensitive ? store.Name(entry) : store.NameLower(entry);
                byte rank;
                if (nameCmp == q) rank = 1;
                else if (nameCmp.StartsWith(q, StringComparison.Ordinal)) rank = 2;
                else if (nameCmp.Contains(q, StringComparison.Ordinal)) rank = 3;
                else return ((uint)0, (byte)0, false);
                return ((uint)idx, rank, true);
            })
            .Where(x => x.Item3)
            .Select(x => (Idx: x.Item1, Rank: x.Item2))
            .ToList();

        candidates.Sort((a, b) => a.Rank.CompareTo(b.Rank));
        var overshoot = Math.Max(limit * 5, 1000);
        if (candidates.Count > overshoot) candidates.RemoveRange(overshoot, candidates.Count - overshoot);

        var results = new List<SearchResult>(limit);
        foreach (var (idx, baseRank) in candidates)
        {
            var entry = entries[(int)idx];
            var fullPath = BuildPath(entry.FileRef, store);

            if (excludedDirs.Count != 0)
            {
                var pathLower = fullPath.ToLowerInvariant();
                if (excludedDirs.Any(pathLower.StartsWith)) continue;
            }

            var rank = baseRank;
            var nameLower = store.NameLower(entry);
            if (baseRank <= 2)
            {
                var ext = nameLower.Split('.').LastOrDefault();
                if (ext is not null && AppExtensions.Contains(ext))
                {
                    var pathLower = fullPath.ToLowerInvariant();
                    if (AppPathMarkers.Any(pathLower.Contains)) rank = 0;
                }
            }

            results.Add(new SearchResult(fullPath, store.Name(entry), rank, entry.IsDir));
        }

        results.Sort((a, b) => a.Rank.CompareTo(b.Rank));
        if (results.Count > limit) results.RemoveRange(limit, results.Count - limit);
        return results;
    }

    public static string BuildPath(ulong fileRef, IndexStore store)
    {
        var components = new List<string>(16);
        var current = fileRef;

        for (var i = 0; i < 64; i++)
        {
            var idx = store.LookupIdx(current);
            if (idx is null) break;

            var entry = store.Entries[(int)idx.Value];
            components.Add(store.Name(entry));
            if (entry.ParentRef == current) break;
            current = entry.ParentRef;
        }

        components.Reverse();
        var path = store.DriveRoot;
        foreach (var comp in components) path = Path.Combine(path, comp);
        return path;
    }
}
