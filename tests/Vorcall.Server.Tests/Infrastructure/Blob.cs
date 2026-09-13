using System.Text;

namespace Vorcall.Server.Tests.Infrastructure;

// Bodies that are not pictures. An attachment takes any file of any type, so the suite needs
// fixtures the image rules would have refused, and a streamed file needs a body long enough to
// take a range out of the middle of.
internal static class Blob
{
    // Structurally a PDF rather than the four bytes of its header: an endpoint that ever grew a
    // sniffer would still see a real file here.
    public static byte[] Pdf()
    {
        var body = new StringBuilder();
        body.Append("%PDF-1.4\n");
        body.Append("1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
        body.Append("2 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");
        body.Append("trailer\n<< /Root 1 0 R >>\n%%EOF\n");
        return Encoding.ASCII.GetBytes(body.ToString());
    }

    public static byte[] Text(string text) => Encoding.UTF8.GetBytes(text);

    // Deterministic bytes: the value at an index is a function of that index alone, so a range
    // that came back from the wrong offset does not compare equal to the range that was asked for.
    public static byte[] Of(int size)
    {
        var bytes = new byte[size];
        for (var i = 0; i < size; i++)
        {
            bytes[i] = (byte)(((i * 31) + ((i >> 8) * 17) + (i >> 16)) & 0xFF);
        }

        return bytes;
    }
}
