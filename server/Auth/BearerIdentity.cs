using System.Globalization;
using System.Security.Claims;

namespace Vorcall.Server.Auth;

// The bearer token is the whole identity: sub carries the user id, name the display casing.
// Claims are read by their wire names because JwtBearer's inbound claim mapping is off.
public static class BearerIdentity
{
    public static bool TryGetUserId(ClaimsPrincipal principal, out long userId)
        => long.TryParse(principal.FindFirstValue("sub"), CultureInfo.InvariantCulture, out userId);

    public static string GetUsername(ClaimsPrincipal principal) => principal.FindFirstValue("name") ?? string.Empty;
}
