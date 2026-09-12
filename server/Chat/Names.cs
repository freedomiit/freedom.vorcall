using System.Text;

namespace Vorcall.Server.Chat;

// The name grammars of PROTOCOL.md § Limits, shared by channels, categories, roles, the server
// and the profile fields: counted in Unicode scalars, after trimming.
public static class Names
{
    public const int NameMaxScalars = 32;

    // Topic, description and ban reason.
    public const int LongMaxScalars = 256;

    public const int EmojiMaxScalars = 2;

    // A role icon is one emoji, which can be a base plus a modifier or a variation selector: the
    // byte cap is what keeps such a pair inside icon_emoji's 16 characters.
    public const int EmojiMaxBytes = 16;

    public static bool TryNormalize(string? raw, out string name)
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

    // Empty is a value here — clearing a topic or sending no ban reason — so a null reads as the
    // absent text it is rather than as a rejection.
    public static bool TryNormalizeLong(string? raw, out string text)
    {
        text = string.Empty;
        if (raw is null)
        {
            return true;
        }

        var trimmed = raw.Trim();
        var scalars = 0;
        foreach (var rune in trimmed.EnumerateRunes())
        {
            if (Rune.IsControl(rune))
            {
                return false;
            }

            if (++scalars > LongMaxScalars)
            {
                return false;
            }
        }

        text = trimmed;
        return true;
    }

    // Empty means "no emoji icon", like TryNormalizeLong.
    public static bool TryNormalizeEmoji(string? raw, out string emoji)
    {
        emoji = string.Empty;
        if (raw is null)
        {
            return true;
        }

        var trimmed = raw.Trim();
        var scalars = 0;
        foreach (var rune in trimmed.EnumerateRunes())
        {
            if (Rune.IsControl(rune))
            {
                return false;
            }

            if (++scalars > EmojiMaxScalars)
            {
                return false;
            }
        }

        if (Encoding.UTF8.GetByteCount(trimmed) > EmojiMaxBytes)
        {
            return false;
        }

        emoji = trimmed;
        return true;
    }
}
