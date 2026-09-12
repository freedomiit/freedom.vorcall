using System.Globalization;
using System.Text;

namespace Vorcall.Server.Chat;

// Limits are the ones in PROTOCOL.md: counted in Unicode scalars, after trimming.
public static class Validation
{
    public const int TextMaxScalars = 2000;

    // A bigint id at its widest; 19 digits can still overflow, which is what the parse catches.
    public const int ChannelIdMaxDigits = 19;

    public static bool TryNormalizeText(string? raw, out string text)
    {
        text = string.Empty;
        if (raw is null)
        {
            return false;
        }

        var trimmed = raw.Trim();
        var scalars = 0;
        foreach (var _ in trimmed.EnumerateRunes())
        {
            if (++scalars > TextMaxScalars)
            {
                return false;
            }
        }

        if (scalars == 0)
        {
            return false;
        }

        text = trimmed;
        return true;
    }

    // Channel ids are identity values, so only a positive number can name one: 0 is the wire's
    // "absent" and no longer means general.
    public static bool TryParseChannelId(long raw, out long id)
    {
        id = raw > 0 ? raw : 0;
        return id > 0;
    }

    // The query-string form. Digits only: a sign, spaces or a non-ASCII digit name no channel
    // rather than being normalised into one.
    public static bool TryParseChannelId(string? raw, out long id)
    {
        id = 0;
        if (raw is null || raw.Length is 0 or > ChannelIdMaxDigits)
        {
            return false;
        }

        foreach (var c in raw)
        {
            if (c is < '0' or > '9')
            {
                return false;
            }
        }

        return long.TryParse(raw, NumberStyles.None, CultureInfo.InvariantCulture, out var parsed)
            && TryParseChannelId(parsed, out id);
    }

    // @everyone and @here are literal words, not <@id> tokens. A match needs a word boundary on
    // both sides, so "email@everyone.com" is an address and not a mention; the comparison is
    // case-sensitive, as PROTOCOL.md § Messages has it.
    public static (bool Everyone, bool Here) MentionFlags(string text)
        => (ContainsWord(text, "@everyone"), ContainsWord(text, "@here"));

    private static bool ContainsWord(string text, string word)
    {
        for (var from = 0; from <= text.Length - word.Length;)
        {
            var at = text.IndexOf(word, from, StringComparison.Ordinal);
            if (at < 0)
            {
                return false;
            }

            var end = at + word.Length;
            var before = at == 0 || char.IsWhiteSpace(text[at - 1]);
            var after = end == text.Length
                || char.IsWhiteSpace(text[end])
                || text[end] is '.' or ',' or '!' or '?' or ';' or ':';
            if (before && after)
            {
                return true;
            }

            from = at + 1;
        }

        return false;
    }
}
