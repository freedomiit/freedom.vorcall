using System.Net;
using System.Net.Sockets;

namespace Vorcall.Server.Api;

// Who is allowed to ask the operational endpoints anything. nginx has no location for them, so
// the only ways one can be reached are the host's own loopback, the docker network and a private
// LAN; anything else is a scan and gets the same answer as a path that does not exist.
public static class PrivateSource
{
    public static bool IsPrivate(HttpContext context) => IsPrivate(context.Connection.RemoteIpAddress);

    public static bool IsPrivate(IPAddress? address)
    {
        // No peer at all is the in-process test host, which is as local as a caller gets.
        if (address is null)
        {
            return true;
        }

        if (IPAddress.IsLoopback(address))
        {
            return true;
        }

        // ::ffff:10.0.0.1 and 10.0.0.1 are the same host; which form arrives depends on the
        // listening socket, so the mapped form is unwrapped before anything is compared.
        if (address.IsIPv4MappedToIPv6)
        {
            address = address.MapToIPv4();
        }

        return address.AddressFamily switch
        {
            AddressFamily.InterNetwork => IsPrivateV4(address.GetAddressBytes()),

            // fc00::/7 unique local and fe80::/10 link local, the IPv6 counterparts of the above.
            AddressFamily.InterNetworkV6 => address.IsIPv6UniqueLocal || address.IsIPv6LinkLocal,
            _ => false,
        };
    }

    private static bool IsPrivateV4(byte[] octets) => octets[0] switch
    {
        10 => true,
        127 => true,
        169 => octets[1] == 254,
        172 => octets[1] is >= 16 and <= 31,
        192 => octets[1] == 168,
        _ => false,
    };
}
