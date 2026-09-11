using System.Globalization;
using System.Text.RegularExpressions;
using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;

namespace Vorcall.Server.Chat;

// Mentions on the wire are the token <@user_id>; the server never parses usernames out of text.
// A token whose id names no account stays plain text and is not stored.
public static partial class Mentions
{
    public const string TokenPattern = @"<@(\d{1,19})>";

    public static IReadOnlyList<long> ExtractCandidates(string text)
    {
        var ids = new List<long>();
        foreach (Match match in Token().Matches(text))
        {
            // \d also matches non-ASCII digits, and 19 digits can still overflow: anything that
            // is not a plain long is simply not a mention.
            if (!long.TryParse(match.Groups[1].ValueSpan, NumberStyles.None, CultureInfo.InvariantCulture, out var id))
            {
                continue;
            }

            if (!ids.Contains(id))
            {
                ids.Add(id);
            }
        }

        return ids;
    }

    public static async Task<long[]> ResolveAsync(AppDbContext db, string text)
    {
        var candidates = ExtractCandidates(text);
        if (candidates.Count == 0)
        {
            return [];
        }

        return await db.Users
            .AsNoTracking()
            .Where(u => candidates.Contains(u.Id))
            .Select(u => u.Id)
            .OrderBy(id => id)
            .ToArrayAsync();
    }

    [GeneratedRegex(TokenPattern)]
    private static partial Regex Token();
}
