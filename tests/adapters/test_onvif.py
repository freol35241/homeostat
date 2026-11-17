"""The ONVIF event parsing, without a camera: pull-point replies in, motion out."""

import sys
import unittest
from pathlib import Path
from xml.etree import ElementTree

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "adapters"))

from onvif import SoapError, motion_values, subscription_url


def notification(topic, items):
    simple = "".join(f'<tt:SimpleItem Name="{name}" Value="{value}"/>' for name, value in items)
    return (
        "<wsnt:NotificationMessage>"
        f'<wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">{topic}</wsnt:Topic>'
        "<wsnt:Message><tt:Message>"
        '<tt:Source><tt:SimpleItem Name="VideoSourceConfigurationToken" Value="vsconf"/></tt:Source>'
        f"<tt:Data>{simple}</tt:Data>"
        "</tt:Message></wsnt:Message>"
        "</wsnt:NotificationMessage>"
    )


def pulled(*messages):
    return ElementTree.fromstring(
        '<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"'
        ' xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2"'
        ' xmlns:tt="http://www.onvif.org/ver10/schema"'
        ' xmlns:tev="http://www.onvif.org/ver10/events/wsdl">'
        f"<env:Body><tev:PullMessagesResponse>{''.join(messages)}</tev:PullMessagesResponse></env:Body>"
        "</env:Envelope>"
    )


def created(address):
    return ElementTree.fromstring(
        '<env:Envelope xmlns:env="http://www.w3.org/2003/05/soap-envelope"'
        ' xmlns:tev="http://www.onvif.org/ver10/events/wsdl"'
        ' xmlns:wsa="http://www.w3.org/2005/08/addressing">'
        "<env:Body><tev:CreatePullPointSubscriptionResponse>"
        f"<tev:SubscriptionReference>{address}</tev:SubscriptionReference>"
        "</tev:CreatePullPointSubscriptionResponse></env:Body>"
        "</env:Envelope>"
    )


BASE = "http://cam.lan:2020/onvif/device_service"


class MotionValuesTest(unittest.TestCase):
    def test_each_motion_notification_yields_its_value(self):
        root = pulled(
            notification("tns1:RuleEngine/CellMotionDetector/Motion", [("IsMotion", "true")]),
            notification("tns1:VideoSource/MotionAlarm", [("State", "false")]),
        )
        self.assertEqual(list(motion_values(root)), [(True, None), (False, None)])

    def test_a_notification_on_another_topic_is_skipped(self):
        root = pulled(notification("tns1:Device/Trigger/DigitalInput", [("LogicalState", "true")]))
        self.assertEqual(list(motion_values(root)), [])

    def test_an_unusable_value_yields_an_error(self):
        root = pulled(
            notification("tns1:VideoSource/MotionAlarm", [("State", "1")]),
            notification("tns1:VideoSource/MotionAlarm", [("Other", "true")]),
        )
        self.assertEqual(
            list(motion_values(root)),
            [(None, "unusable motion value '1'"), (None, "unusable motion value None")],
        )


class SubscriptionUrlTest(unittest.TestCase):
    def test_the_configured_address_replaces_the_cameras(self):
        root = created("<wsa:Address> http://192.168.0.5:1025/onvif/Events?sub=3#x </wsa:Address>")
        self.assertEqual(subscription_url(root, BASE), "http://cam.lan:2020/onvif/Events?sub=3")

    def test_a_missing_or_empty_reference_is_a_soap_error(self):
        for address in ("", "<wsa:Address>  </wsa:Address>"):
            with self.subTest(address=address), self.assertRaises(SoapError):
                subscription_url(created(address), BASE)


if __name__ == "__main__":
    unittest.main()
