using System.Text;

namespace Vorcall.Server.Chat;

// Limits are the ones in PROTOCOL.md: counted in Unicode scalars, after trimming.
internal static class Validation
{
    public const int NicknameMaxScalars = 32;
    public const int TextMaxScalars = 2000;

    public static bool TryNormalizeNickname(string? raw, out string nickname)
    {
        nickname = string.Empty;
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

            if (++scalars > NicknameMaxScalars)
            {
                return false;
            }
        }

        if (scalars == 0)
        {
            return false;
        }

        nickname = trimmed;
        return true;
    }

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
}
