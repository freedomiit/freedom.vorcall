using System.Globalization;
using System.Text.RegularExpressions;

namespace Vorcall.Server.Updates;

// Exactly MAJOR.MINOR.PATCH. Anything else — an empty field, a suffix, a local build — is not
// guessed at: it fails to parse, and a version that does not parse counts as outdated.
public readonly partial record struct ClientVersion(int Major, int Minor, int Patch) : IComparable<ClientVersion>
{
    public static bool TryParse(string? raw, out ClientVersion version)
    {
        version = default;
        if (raw is null || !VersionPattern().IsMatch(raw))
        {
            return false;
        }

        var parts = raw.Split('.');
        if (!TryParsePart(parts[0], out var major) || !TryParsePart(parts[1], out var minor) || !TryParsePart(parts[2], out var patch))
        {
            return false;
        }

        version = new ClientVersion(major, minor, patch);
        return true;
    }

    public int CompareTo(ClientVersion other)
    {
        var major = Major.CompareTo(other.Major);
        if (major != 0)
        {
            return major;
        }

        var minor = Minor.CompareTo(other.Minor);
        return minor != 0 ? minor : Patch.CompareTo(other.Patch);
    }

    // The regex admits digits only, so the sole remaining failure is a component too long for an
    // int, which TryParse reports instead of throwing.
    private static bool TryParsePart(string raw, out int value)
        => int.TryParse(raw, NumberStyles.None, CultureInfo.InvariantCulture, out value);

    // \A and \z rather than ^ and $, for the reason spelled out in Chat/Validation.cs: in .NET $
    // also matches just before a trailing newline, which "0.2.0\n" would slip through.
    [GeneratedRegex(@"\A[0-9]+\.[0-9]+\.[0-9]+\z", RegexOptions.CultureInvariant)]
    private static partial Regex VersionPattern();
}
