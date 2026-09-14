import socket
import struct
import unittest
from nx_recall_voice.name_assistance import receive, send


class Frames(unittest.TestCase):
    def test_roundtrip_and_eof(self):
        left, right = socket.socketpair()
        with left, right:
            send(left, b'ready')
            self.assertEqual(receive(right), b'ready')
            left.shutdown(socket.SHUT_WR)
            with self.assertRaises(EOFError):
                receive(right)

    def test_oversized_header_rejected_before_body_read(self):
        left, right = socket.socketpair()
        with left, right:
            left.sendall(struct.pack('!I', 100))
            with self.assertRaises(ValueError):
                receive(right, limit=8)
            with self.assertRaises(ValueError):
                send(left, b'x' * 8193)
