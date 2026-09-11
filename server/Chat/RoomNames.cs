using System.Globalization;
using System.Text;

namespace Vorcall.Server.Chat;

// The room name and slug grammar of PROTOCOL.md § Rooms and presence. A public room's id is the
// slug of its name; a DM's id is derived from its two member ids and never from a name.
public static class RoomNames
{
    public const int NameMaxScalars = 32;
    public const int SlugMaxLength = 32;

    // A public slug may never collide with the DM namespace, which is keyed by user ids.
    public const string DmPrefix = "dm-";

    public static bool TryNormalizeName(string? raw, out string name)
    {
        name = string.Empty;
        if (raw is null)
        {
            return false;
        }

        var trimmed = raw.Trim();
        var scalars = 0;
        foreach (var rune in trimmed.EnumerateRunes())
        {
            if (Rune.IsControl(rune))
            {
                return false;
            }

            if (++scalars > NameMaxScalars)
            {
                return false;
            }
        }

        if (scalars == 0)
        {
            return false;
        }

        name = trimmed;
        return true;
    }

    public static bool TrySlug(string name, out string slug)
    {
        slug = string.Empty;

        var builder = new StringBuilder(name.Length);
        foreach (var rune in name.ToLowerInvariant().EnumerateRunes())
        {
            if (rune.Value is (>= 'a' and <= 'z') or (>= '0' and <= '9'))
            {
                builder.Append((char)rune.Value);
            }
            else if (rune.Value is '-' or '_' || Rune.IsWhiteSpace(rune))
            {
                // Appending only after a kept character, and only once, is what collapses runs
                // and drops a leading separator in the same pass.
                if (builder.Length > 0 && builder[^1] != '-')
                {
                    builder.Append('-');
                }
            }
        }

        if (builder.Length > 0 && builder[^1] == '-')
        {
            builder.Length--;
        }

        var candidate = builder.ToString();
        if (candidate.Length == 0 || candidate.Length > SlugMaxLength || candidate.StartsWith(DmPrefix, StringComparison.Ordinal))
        {
            return false;
        }

        slug = candidate;
        return true;
    }

    public static string DmRoomId(long a, long b)
        => string.Create(CultureInfo.InvariantCulture, $"{DmPrefix}{Math.Min(a, b)}-{Math.Max(a, b)}");

    public static bool IsDm(string roomId) => roomId.StartsWith(DmPrefix, StringComparison.Ordinal);
}
