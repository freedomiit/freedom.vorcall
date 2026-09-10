using System.Security.Cryptography;
using System.Text;

namespace Vorcall.Server.Auth;

public sealed class ServerKeyValidator
{
    private readonly byte[] _keyDigest;

    public ServerKeyValidator(string key)
    {
        _keyDigest = SHA256.HashData(Encoding.UTF8.GetBytes(key));
    }

    // Both sides are hashed before the comparison so a wrong length costs exactly as much as
    // a wrong byte: SHA-256 digests are always 32 bytes, so there is no early length return.
    public bool IsValid(string? presentedKey)
    {
        if (presentedKey is null)
        {
            return false;
        }

        var presentedDigest = SHA256.HashData(Encoding.UTF8.GetBytes(presentedKey));
        return CryptographicOperations.FixedTimeEquals(presentedDigest, _keyDigest);
    }
}
