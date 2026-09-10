using System.Text;
using System.Text.RegularExpressions;

namespace Vorcall.Server.Chat;

// Limits are the ones in PROTOCOL.md: counted in Unicode scalars, after trimming.
internal static partial class Validation
{
    public const int TextMaxScalars = 2000;

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

    // An absent room id means general, on the wire and in the query string alike.
    public static bool TryNormalizeRoomId(string? raw, out string roomId)
    {
        if (string.IsNullOrEmpty(raw))
        {
            roomId = ConnectionRegistry.GeneralRoomId;
            return true;
        }

        if (!RoomIdPattern().IsMatch(raw))
        {
            roomId = string.Empty;
            return false;
        }

        roomId = raw;
        return true;
    }

    // \A and \z rather than ^ and $: in .NET $ also matches just before a trailing newline,
    // which would let "general\n" through the documented ^[a-z0-9-]{1,32}$ grammar.
    [GeneratedRegex(@"\A[a-z0-9-]{1,32}\z")]
    private static partial Regex RoomIdPattern();
}
